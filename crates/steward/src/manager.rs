//! The manager: one thread, waiting on one completion port, turning what
//! happens to processes and jobs into events for each service's state machine
//! and carrying out the actions the machines answer with.
//!
//! Starts and stops are ordered by the plan: `default.target` is reached at
//! once, `graphical-session.target` when Explorer's taskbar exists, and
//! `tray.target` when Explorer says the taskbar is ready. On the way out
//! the manager either stops everything, in reverse order (the SCM stopping
//! the instance, which is what sign-out does; system shutdown; Ctrl+C in a
//! console), or detaches, leaving its services running and their jobs
//! recorded for the next manager to adopt (handing over to a new manager, as
//! an upgrade does).
//!
//! A timer is looked at on every turn of the loop, which comes at least once
//! a second: when one is due, what it starts is started, through the plan
//! like any start. Its schedule is worked out afresh each time, from the
//! wall clock, so time asleep, a clock set right and a new time zone all
//! count at once.

use std::cell::{Cell, OnceCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::hash::{BuildHasher, Hasher};
use std::io::Write;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use steward_cat::etw::{Channel, Level, Provider};
use steward_cat::{utf16, Stream};
use steward_eventlog::{
    channel_name, provider_guid, provider_name, CHANNEL_KEYWORD, CHANNEL_VALUE,
};
use steward_ipc::{ManagerStatus, Request, Response, TimerStatus, UnitStatus};
use steward_supervisor::plan::{DEFAULT_TARGET, GRAPHICAL_TARGET, TIMERS_TARGET, TRAY_TARGET};
use steward_supervisor::{
    Action, Decision, Event as UnitEvent, Machine, Moments, Outcome, Plan, Process, Progress,
    Schedule, State,
};
use steward_unit::{Command, KillMode, Service};
use windows_sys::core::BOOL;
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::SystemServices::{
    JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS, JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO,
    JOB_OBJECT_MSG_EXIT_PROCESS, JOB_OBJECT_MSG_NEW_PROCESS,
};

use crate::control::{self, Refusal};
use crate::log::{error, info, warning};
use crate::state::{
    self, from_millis, to_millis, RestState, Saved, SavedProcess, SavedRest, SavedTimer, SavedUnit,
};
use crate::sys::clock::{self, Local};
use crate::sys::job::Job;
use crate::sys::port::{Packet, Port, Waker};
use crate::sys::process::{self, Child, ExitWatch, Handles};
use crate::sys::{self, env, signal};

/// A nudge: there are controls in the channel.
pub const KEY_WAKE: usize = 1;
/// A process exited; the packet's value is its token.
const KEY_EXIT: usize = 2;
/// Explorer broadcast `TaskbarCreated`: its tray takes icons.
const KEY_TASKBAR_CREATED: usize = 3;
/// A job's notification; the key is this plus the job's serial number.
const KEY_JOB_BASE: usize = 0x1_0000;

/// The longest the manager waits before looking at the world again.
const TICK: Duration = Duration::from_secs(1);
/// How often to look for the shell until it is there.
const SHELL_POLL: Duration = Duration::from_millis(250);
/// How long after Explorer starts its tray surely takes icons, `TaskbarCreated`
/// or not: for a manager that started too late to hear it, or one that never
/// does. It took under 2 s on gaming-windows, at sign-in and at a restart.
const TRAY_GRACE: Duration = Duration::from_secs(10);
/// How often the units' logs are measured against [`crate::log::CAP`]
/// during the run. A unit that logs steadily and never starts again would
/// otherwise fill the disk.
const LOG_CHECK: Duration = Duration::from_secs(10);
/// The exit code steward terminates processes with.
const KILLED: u32 = 0x5354_5744; // "STWD"
/// NTSTATUS for "unsuccessful": a crash whose exit code could not be read.
const STATUS_UNSUCCESSFUL: u32 = 0xC000_0001;

/// What the SCM's or the console's control handler tells the manager.
pub enum Control {
    /// Leave the services running and exit; the next manager adopts them.
    Detach(String),
    /// Stop every service, in order, then exit.
    StopAll(String),
    /// Something worth a line in the log.
    Note(String),
    /// A `stewctl` request, and where its answer goes.
    Request(Request, Sender<Response>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Exit {
    Detach,
    StopAll,
}

struct Tracked {
    child: Child,
    token: usize,
    _watch: ExitWatch,
}

/// Where a unit's output goes for one run: decided at the run's first
/// spawn, let go of with its job.
enum Output {
    /// The user's Event Log channel, through a `steward-cat` the manager
    /// started for the unit. The manager holds the write ends of the two
    /// pipes to it and every process it starts for the unit inherits them;
    /// the shim reads until every write end is closed -- the manager's with
    /// the job, or with the manager itself, and the processes' as they exit
    /// -- so it outlives a manager crash or hand-over for exactly as long as
    /// the unit does, and dies with it.
    ///
    /// It is not in the unit's job. The pipe is what ties the shim to the
    /// unit, and the job would only get in the way: the job would never be
    /// empty while the shim ran; a stop that terminates the job would take
    /// the shim with it and lose the unit's last lines, the ones worth
    /// reading; and the Ctrl+C that asks the job's processes to exit would
    /// end the shim first.
    EventLog {
        stdout: OwnedHandle,
        stderr: OwnedHandle,
        shim: Tracked,
    },
    /// A `StandardOutput=eventlog` unit whose `steward-cat` could not be
    /// started, or exited: its output goes to its file for this run, and
    /// this is why.
    Fallback(String),
}

struct Unit {
    name: String,
    /// Its unit file is gone; it is forgotten once it is at rest.
    removed: bool,
    /// When it entered its current state, and that time as text.
    since: (Instant, String),
    machine: Machine,
    job: Option<Job>,
    job_serial: usize,
    /// JobEmpty has been fed since the last process was started.
    job_empty_fed: bool,
    main: Option<Tracked>,
    control: Option<Tracked>,
    /// A timer's schedule; never started for anything else.
    schedule: Schedule,
    /// When it last left rest, and last came to rest: what a timer that
    /// starts it counts `OnUnitActiveSec=` and `OnUnitInactiveSec=` from.
    started_at: Option<SystemTime>,
    stopped_at: Option<SystemTime>,
    /// The last manager left it at rest: reaching a target passes over it,
    /// as that manager's reaching the target did. Until it starts again.
    left_at_rest: bool,
    /// Its log could not be set aside, and that has been said: once, not
    /// every time it is tried again.
    log_warned: bool,
    /// Where this run's output goes, once a run has begun. `None` for a
    /// `StandardOutput=file` unit, which opens its log at each spawn, and
    /// for an eventlog unit adopted from another manager, whose pipes and
    /// shim were that manager's.
    output: Option<Output>,
}

impl Unit {
    fn new(service: Service) -> Unit {
        Unit {
            name: service.name.clone(),
            removed: false,
            since: (Instant::now(), crate::log::timestamp()),
            machine: Machine::new(service),
            job: None,
            job_serial: 0,
            job_empty_fed: true,
            main: None,
            control: None,
            schedule: Schedule::default(),
            started_at: None,
            stopped_at: None,
            left_at_rest: false,
            log_warned: false,
            output: None,
        }
    }

    fn progress(&self) -> Progress {
        Progress::from(self.machine.state())
    }

    fn resting(&self) -> bool {
        matches!(
            self.machine.state(),
            State::Inactive | State::Failed | State::AutoRestart
        )
    }

    /// Where it rests, for the next manager, if it has run (or been refused)
    /// in this sign-in. One that has not is left to whatever reaches it.
    fn rest(&self) -> Option<SavedRest> {
        let state = match self.machine.state() {
            State::Inactive => RestState::Inactive,
            State::Failed => RestState::Failed,
            State::AutoRestart => RestState::AutoRestart,
            _ => return None,
        };
        let outcome = self.machine.last_outcome();
        let ran = self.left_at_rest || self.stopped_at.is_some() || outcome.is_some();
        (ran && !self.removed).then(|| SavedRest {
            state,
            outcome: outcome.map(Into::into),
            last_trigger: self.schedule.last_trigger.map(to_millis),
        })
    }
}

struct Manager {
    port: Port,
    inbox: Receiver<Control>,
    units: Vec<Unit>,
    plan: Plan,
    to_start: BTreeSet<String>,
    to_stop: BTreeSet<String>,
    graphical: bool,
    tray: bool,
    /// Listening for `TaskbarCreated` since before the taskbar existed, so
    /// the broadcast that readies the tray cannot have been missed.
    hears_taskbar_created: bool,
    /// When the tray counts as ready without the broadcast: Explorer's start
    /// and [`TRAY_GRACE`], once the taskbar is seen.
    tray_deadline: Option<Instant>,
    exit: Option<Exit>,
    next_token: usize,
    next_serial: usize,
    dirty: bool,
    state_dir: Option<PathBuf>,
    /// The session this manager belongs to, and its services with it.
    session: u32,
    /// When the session's user signed in, if Windows says: which sign-in
    /// the state file is about.
    logon: Option<SystemTime>,
    /// The rests as they were when everything began to be stopped. A stop
    /// of everything is not a stop of each unit, and does not outlast itself.
    rests_before_stop_all: Option<BTreeMap<String, SavedRest>>,
    stdin: Option<OwnedHandle>,
    /// When Windows started, and when the session did: what `OnBootSec=`
    /// and `OnStartupSec=` count from.
    boot: SystemTime,
    startup: SystemTime,
    /// The earliest a timer is next due, as last worked out.
    next_elapse: Option<SystemTime>,
    /// When the units' logs were last measured.
    logs_checked: Instant,
    /// The manager's own registration of the user's Event Log provider,
    /// made the first time a `StandardOutput=eventlog` unit has a line to
    /// be written; `None` inside once registering has failed.
    provider: OnceCell<Option<Provider>>,
    /// Nobody listens to the provider, and that has been said: once, until
    /// somebody does again.
    channel_quiet: Cell<bool>,
}

/// How a manager ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// It was told to stop or to detach, running or still waiting for the
    /// pipe.
    Stopped,
    /// It did not run: another manager of this user serves the session.
    Yielded,
    /// It did not run: the control pipe cannot be served.
    Failed,
}

/// The longest wait between attempts to create a pipe someone else holds.
const PIPE_RETRY_MAX: Duration = Duration::from_secs(60);

/// Run the manager until it is told to stop or detach. `controls` is the
/// sending end of `inbox`, for the control plane.
pub fn run(port: Port, controls: Sender<Control>, inbox: Receiver<Control>) -> Ending {
    let session = sys::own_session();
    info!(
        "steward {} starting in session {session}",
        env!("CARGO_PKG_VERSION")
    );
    // The pipe is also the lock: one manager per session.
    if let Err(ending) = serve_pipe(&controls, &port, &inbox) {
        return ending;
    }
    let logon = clock::logon_time(session);
    let mut manager = Manager {
        port,
        inbox,
        units: Vec::new(),
        plan: Plan::default(),
        to_start: BTreeSet::new(),
        to_stop: BTreeSet::new(),
        graphical: false,
        tray: false,
        hears_taskbar_created: false,
        tray_deadline: None,
        exit: None,
        next_token: 1,
        next_serial: 1,
        dirty: false,
        state_dir: crate::log::state_dir(),
        session,
        logon,
        rests_before_stop_all: None,
        stdin: process::open_null()
            .map_err(|e| error!("cannot open NUL for services' stdin: {e}"))
            .ok(),
        boot: clock::boot_time(),
        // Sign-in; failing that, now, which is as near as the manager knows.
        startup: logon.unwrap_or_else(SystemTime::now),
        next_elapse: None,
        logs_checked: Instant::now(),
        provider: OnceCell::new(),
        channel_quiet: Cell::new(false),
    };
    manager.load_units();
    manager.adopt();
    let wanted = manager.plan.pulled_in_by(DEFAULT_TARGET);
    manager.want_started(wanted);
    manager.watch_taskbar_created();
    manager.check_shell();
    manager.run();
    Ending::Stopped
}

/// Create the pipe and serve it. A name held by another manager of this
/// user's is theirs to keep; one held by anyone else is waited out, trying
/// again with a growing delay until it is free or a control says to stop.
/// The SCM starts a manager once per sign-in and does not retry a clean
/// stop, so a manager that gave up would leave the session without one for
/// as long as the squatter cared to stay.
fn serve_pipe(
    controls: &Sender<Control>,
    port: &Port,
    inbox: &Receiver<Control>,
) -> Result<(), Ending> {
    let mut delay = Duration::from_secs(1);
    let mut attempts = 0u32;
    loop {
        let why = match control::listen(controls.clone(), port.waker()) {
            Ok(()) => return Ok(()),
            Err(Refusal::Held) => {
                info!("another steward serves this session already; not starting");
                return Err(Ending::Yielded);
            }
            Err(Refusal::Failed(e)) => {
                error!("cannot serve the control pipe: {e}; not starting");
                return Err(Ending::Failed);
            }
            Err(Refusal::Taken(why)) => why,
        };
        attempts += 1;
        // Every attempt while the delay grows, then one in ten: a name held
        // for good would otherwise fill the log.
        if delay < PIPE_RETRY_MAX || attempts.is_multiple_of(10) {
            error!(
                "cannot create the control pipe: {why}; trying again in {} s",
                delay.as_secs()
            );
        }
        let until = Instant::now() + delay;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match inbox.recv_timeout(left) {
                Ok(Control::StopAll(reason)) | Ok(Control::Detach(reason)) => {
                    info!("{reason}: giving up on the control pipe");
                    return Err(Ending::Stopped);
                }
                Ok(Control::Note(note)) => info!("{note}"),
                Ok(Control::Request(_, reply)) => {
                    let _ = reply.send(Response::error("steward has no control pipe yet"));
                }
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        delay = (delay * 2).min(PIPE_RETRY_MAX);
    }
}

impl Manager {
    fn load_units(&mut self) {
        match read_units() {
            Ok((services, messages)) => {
                for message in messages {
                    warning!("{message}");
                }
                for service in services {
                    self.units.push(Unit::new(service));
                }
            }
            Err(e) => error!("{e}"),
        }
        self.replan();
    }

    fn replan(&mut self) -> Vec<String> {
        let (plan, warnings) = Plan::new(
            self.units
                .iter()
                .filter(|u| !u.removed)
                .map(|u| u.machine.next_service()),
        );
        for warning in &warnings {
            warning!("{warning}");
        }
        self.plan = plan;
        warnings
    }

    fn slot(&self, name: &str) -> Option<usize> {
        self.units.iter().position(|u| u.name == name)
    }

    fn progress(&self, name: &str) -> Progress {
        self.slot(name)
            .map_or(Progress::Idle, |s| self.units[s].progress())
    }

    fn want_started(&mut self, names: BTreeSet<String>) {
        for name in names {
            let left_at_rest = self.slot(&name).is_some_and(|s| self.units[s].left_at_rest);
            if self.progress(&name) == Progress::Idle && !left_at_rest {
                self.to_start.insert(name);
            }
        }
    }

    // ---- requests from stewctl ------------------------------------------

    fn answer(&mut self, request: Request) -> Response {
        let units = match &request {
            Request::Status { units }
            | Request::Start { units }
            | Request::Stop { units }
            | Request::Restart { units } => units.clone(),
            Request::Reload { .. } => Vec::new(),
        };
        let mut slots = Vec::new();
        for name in &units {
            match self.slot(name).filter(|&s| !self.units[s].removed) {
                Some(slot) => slots.push(slot),
                None => return Response::error(format!("no such unit: {name}")),
            }
        }
        let mut messages = Vec::new();
        match request {
            Request::Status { units } => {
                if units.is_empty() {
                    slots = (0..self.units.len())
                        .filter(|&s| !self.units[s].removed)
                        .collect();
                }
                return Response {
                    manager: Some(self.manager_status()),
                    units: slots.into_iter().map(|s| self.unit_status(s)).collect(),
                    ..Response::default()
                };
            }
            Request::Reload { apply } => return self.reload(apply),
            Request::Start { .. } | Request::Restart { .. } if self.exit.is_some() => {
                return Response::error("steward is shutting down");
            }
            Request::Start { units } => {
                for name in units {
                    messages.extend(self.start_with_dependencies(&name));
                }
            }
            Request::Stop { units } => {
                for name in units {
                    messages.extend(self.stop_with_bound(&name));
                }
            }
            Request::Restart { .. } => {
                for slot in slots {
                    messages.extend(self.restart(slot));
                }
            }
        }
        Response {
            messages,
            ..Response::default()
        }
    }

    /// Queue `name` and what it wants or requires for starting, if they are
    /// not running already.
    fn start_with_dependencies(&mut self, name: &str) -> Vec<String> {
        let mut messages = Vec::new();
        for unit in self.plan.with_dependencies(name) {
            let Some(slot) = self.slot(&unit) else {
                continue;
            };
            match self.units[slot].machine.state() {
                State::Inactive | State::Failed => {
                    self.to_start.insert(unit.clone());
                    messages.push(format!("{unit}: starting"));
                }
                // Skip the rest of the delay -- but through the plan, like any
                // start, so the unit still waits for what it is ordered
                // after: its pending restart is cancelled (its delay would
                // otherwise start it on its own) and it is queued. Started
                // on request, it starts over from the first restart delay.
                State::AutoRestart => {
                    self.feed(slot, UnitEvent::Stop);
                    self.to_start.insert(unit.clone());
                    messages.push(format!("{unit}: starting now instead of after its delay"));
                }
                _ if unit == name => messages.push(format!("{unit}: already running")),
                _ => {}
            }
        }
        messages
    }

    /// Stop `name`, and what requires it or is part of it -- as a stop asked
    /// for does in systemd.
    fn stop_with_bound(&mut self, name: &str) -> Vec<String> {
        let mut messages = Vec::new();
        let bound = self.plan.bound_to(name);
        for unit in std::iter::once(name.to_owned()).chain(bound) {
            let Some(slot) = self.slot(&unit) else {
                continue;
            };
            self.to_start.remove(&unit);
            let state = self.units[slot].machine.state();
            // Already at rest; but one waiting out a restart delay is on its
            // way back, and the stop cancels that.
            if unit != name && matches!(state, State::Inactive | State::Failed) {
                continue;
            }
            self.feed(slot, UnitEvent::Stop);
            messages.push(match (unit == name, state) {
                (true, _) => format!("{unit}: stopping"),
                (false, State::AutoRestart) => format!("{unit}: not restarting, with {name}"),
                (false, _) => format!("{unit}: stopping, with {name}"),
            });
        }
        messages
    }

    /// Restart a unit, and what requires it or is part of it and runs.
    fn restart(&mut self, slot: usize) -> Vec<String> {
        let name = self.units[slot].name.clone();
        let mut messages = self.restart_one(slot);
        for unit in self.plan.bound_to(&name) {
            match self.slot(&unit) {
                Some(bound) if !self.units[bound].resting() => {
                    messages.extend(self.restart_one(bound));
                }
                _ => {}
            }
        }
        messages
    }

    fn restart_one(&mut self, slot: usize) -> Vec<String> {
        let name = self.units[slot].name.clone();
        if self.units[slot].resting() {
            return self.start_with_dependencies(&name);
        }
        // A running unit restarts through its stop: the machine starts it
        // again once the stop is done, with its newest definition. (A target
        // is stopped and started at once.)
        self.feed(slot, UnitEvent::Stop);
        self.feed(slot, UnitEvent::Start);
        vec![format!("{name}: restarting")]
    }

    /// What the reached targets pull in.
    fn wanted(&self) -> BTreeSet<String> {
        let mut wanted = self.plan.pulled_in_by(DEFAULT_TARGET);
        if self.graphical {
            wanted.extend(self.plan.pulled_in_by(GRAPHICAL_TARGET));
        }
        if self.tray {
            wanted.extend(self.plan.pulled_in_by(TRAY_TARGET));
        }
        wanted
    }

    /// Read the unit files again; with `apply`, make what runs match them, as
    /// sd-switch does for home-manager: the changed that run restart, the
    /// removed stop, and of those at rest only what is new starts -- a new
    /// unit, one a target newly wants, or a failed one whose definition
    /// changed. A unit stopped on purpose stays stopped.
    fn reload(&mut self, apply: bool) -> Response {
        let (services, mut messages) = match read_units() {
            Ok(read) => read,
            Err(e) => return Response::error(e),
        };
        let wanted_before = self.wanted();
        let mut fresh: std::collections::BTreeMap<String, Service> =
            services.into_iter().map(|s| (s.name.clone(), s)).collect();

        for slot in 0..self.units.len() {
            let unit = &mut self.units[slot];
            if unit.removed || fresh.contains_key(&unit.name) {
                continue;
            }
            unit.removed = true;
            let name = unit.name.clone();
            self.to_start.remove(&name);
            if self.units[slot].resting() {
                messages.push(format!("{name}: removed"));
            } else {
                messages.push(format!("{name}: removed; stopping it"));
            }
            // Stopping a unit at rest cancels any restart it is waiting for.
            self.feed(slot, UnitEvent::Stop);
        }

        let mut changed = Vec::new();
        let mut added = Vec::new();
        for (name, service) in std::mem::take(&mut fresh) {
            match self.slot(&name) {
                Some(slot) => {
                    let unit = &mut self.units[slot];
                    let came_back = std::mem::take(&mut unit.removed);
                    if came_back || *unit.machine.next_service() != service {
                        unit.machine.replace(service);
                        messages.push(format!("{name}: changed"));
                        if came_back {
                            added.push(name.clone());
                        }
                        changed.push(name);
                    }
                }
                None => {
                    messages.push(format!("{name}: new"));
                    self.units.push(Unit::new(service));
                    added.push(name.clone());
                    changed.push(name);
                }
            }
        }
        messages.extend(self.replan());

        if apply && self.exit.is_none() {
            // Changed by this reload or an earlier one without --apply: what
            // runs differs from what is on disk.
            for unit in &self.units {
                if !unit.removed && unit.machine.is_changed() && !changed.contains(&unit.name) {
                    changed.push(unit.name.clone());
                }
            }
            for name in &changed {
                let Some(slot) = self.slot(name) else {
                    continue;
                };
                // A target took its new definition at once, and restarting
                // it would restart what is part of it: an edited description
                // is no reason to bounce a whole group. A timer took its new
                // definition at once too, and its next elapse follows it.
                let unit = &self.units[slot];
                if !unit.resting() && !unit.machine.service().runs_nothing() {
                    messages.extend(self.restart(slot));
                }
            }
            for name in self.wanted() {
                let Some(slot) = self.slot(&name) else {
                    continue;
                };
                let start = match self.units[slot].machine.state() {
                    State::Inactive => added.contains(&name) || !wanted_before.contains(&name),
                    // Tried again once its definition changes.
                    State::Failed => changed.contains(&name),
                    _ => false,
                };
                if start {
                    self.to_start.insert(name.clone());
                    messages.push(format!("{name}: starting"));
                }
            }
        }
        info!(
            "reloaded the units{}",
            if apply { " and applied them" } else { "" }
        );
        Response {
            messages,
            ..Response::default()
        }
    }

    /// Units whose files are gone, once they are at rest.
    fn forget_removed(&mut self) {
        self.units
            .retain(|u| !(u.removed && u.resting() && u.job.is_none() && u.main.is_none()));
    }

    fn manager_status(&self) -> ManagerStatus {
        let text = |p: Option<PathBuf>| p.map(|p| p.display().to_string()).unwrap_or_default();
        ManagerStatus {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            pid: std::process::id(),
            session: self.session,
            graphical_session: self.graphical,
            tray: self.tray,
            unit_dir: text(steward_unit::user_unit_dir()),
            log_dir: text(self.state_dir.as_ref().map(|d| d.join("logs"))),
        }
    }

    fn unit_status(&self, slot: usize) -> UnitStatus {
        let unit = &self.units[slot];
        let service = unit.machine.service();
        let state = unit.machine.state();
        let now = Instant::now();
        UnitStatus {
            name: unit.name.clone(),
            description: service.description.clone(),
            path: steward_unit::user_unit_dir()
                .map(|d| d.join(&unit.name).display().to_string())
                .unwrap_or_default(),
            state: state.name().to_owned(),
            main_pid: unit.main.as_ref().map(|t| t.child.pid),
            pids: unit
                .job
                .as_ref()
                .and_then(|j| j.pids().ok())
                .unwrap_or_default(),
            restarts: unit.machine.restarts(),
            last_outcome: unit.machine.last_outcome().map(|o| o.to_string()),
            since: Some(unit.since.1.clone()),
            for_secs: Some(now.duration_since(unit.since.0).as_secs()),
            restart_in_secs: (state == State::AutoRestart)
                .then(|| unit.machine.deadline())
                .flatten()
                .map(|d| d.saturating_duration_since(now).as_secs_f64()),
            wanted_by: service.wanted_by.clone(),
            changed: unit.machine.is_changed(),
            timer: self.timer_status(slot),
            output_fallback: match &unit.output {
                Some(Output::Fallback(why)) => Some(why.clone()),
                _ => None,
            },
        }
    }

    fn timer_status(&self, slot: usize) -> Option<TimerStatus> {
        let unit = &self.units[slot];
        let timer = unit.machine.service().timer.as_ref()?;
        let zone = Local::current();
        let now = SystemTime::now();
        let next = self.next_elapse_of(slot, now, &zone);
        let schedule = &unit.schedule;
        let state = match unit.machine.state() {
            State::Active if schedule.running => "running",
            State::Active if next.is_some() => "waiting",
            State::Active => "elapsed",
            _ => "",
        };
        Some(TimerStatus {
            unit: timer.unit.clone(),
            state: state.to_owned(),
            next: next.map(|t| clock::format(t, &zone)),
            next_in_secs: next.map(|t| match t.duration_since(now) {
                Ok(left) => left.as_secs_f64(),
                Err(overdue) => -overdue.duration().as_secs_f64(),
            }),
            last: schedule.last_trigger.map(|t| clock::format(t, &zone)),
            last_secs_ago: schedule
                .last_trigger
                .map(|t| now.duration_since(t).map_or(0, |d| d.as_secs())),
        })
    }

    // ---- adoption -------------------------------------------------------

    /// Take back the services a previous manager in this session left
    /// running, and leave what it left at rest there.
    fn adopt(&mut self) {
        let Some(path) = self.state_path() else {
            return;
        };
        let mut saved = state::load(&path).unwrap_or_else(|e| {
            warning!("ignoring the saved state: {e}");
            Saved::default()
        });
        // A session number is reused by a later sign-in, whose manager starts
        // afresh. Only processes are still taken back: they are their own
        // proof, and one that still runs here is this session's.
        if !saved.same_sign_in(self.logon.map(to_millis)) {
            info!("the saved state is from an earlier sign-in; starting afresh");
            saved.targets.clear();
            saved.timers.clear();
            saved.rests.clear();
        }
        for (name, record) in saved.units {
            // The recorded processes that are still the same processes, and
            // still in this session. A process in another session belongs to
            // that session -- one signing out, perhaps, whose processes are
            // about to be ended -- and is not this manager's to take.
            let (alive, elsewhere): (Vec<Child>, Vec<Child>) = record
                .processes
                .iter()
                .filter_map(|p| Child::open(p.pid, p.created).ok())
                .partition(|c| sys::session_of(c.pid) == Some(self.session));
            if !elsewhere.is_empty() {
                let pids: Vec<u32> = elsewhere.iter().map(|c| c.pid).collect();
                warning!("{name}: processes {pids:?} are in another session; leaving them to it");
            }
            if alive.is_empty() {
                continue;
            }
            let Some(slot) = self.slot(&name) else {
                warning!("{name} is still running but has no unit any more; stopping it");
                for child in &alive {
                    let _ = child.terminate(KILLED);
                }
                continue;
            };
            // The old job has no handle left, and so no name; a new one takes
            // its processes, nested inside it.
            if let Err(e) = self.new_job(slot) {
                error!("{name}: cannot make a job to adopt it into: {e}");
                continue;
            }
            let job = self.units[slot].job.as_ref().expect("just made");
            let mut main_child = None;
            for child in alive {
                if let Err(e) = job.assign(child.raw()) {
                    warning!(
                        "{name}: cannot take process {} into its job: {e}",
                        child.pid
                    );
                }
                if record.main.is_some_and(|m| m.pid == child.pid) {
                    main_child = Some(child);
                }
            }
            let main = main_child.and_then(|child| self.track(child).ok());
            let pid = main.as_ref().map(|t| t.child.pid);
            let unit = &mut self.units[slot];
            unit.job_empty_fed = false;
            unit.main = main;
            // The last manager's pipes and shim went with it; its fallback,
            // if it had one, is still where the processes write.
            unit.output = record.output_fallback.clone().map(Output::Fallback);
            match pid {
                Some(pid) => info!("{name}: adopted, main process {pid}"),
                None => info!("{name}: adopted what is left of it; the main process is gone"),
            }
            let actions = self.units[slot]
                .machine
                .adopt(pid.is_some(), Instant::now());
            self.carry_out(slot, actions);
        }
        for name in saved.targets {
            let Some(slot) = self
                .slot(&name)
                .filter(|&s| self.units[s].machine.service().is_target())
            else {
                continue;
            };
            self.units[slot].machine.adopt(false, Instant::now());
            info!("{name}: active, as it was");
        }
        for (name, record) in saved.timers {
            let Some(slot) = self.slot(&name) else {
                continue;
            };
            let Some(timer) = self.units[slot].machine.service().timer.clone() else {
                continue;
            };
            // What the unit it starts last did, which this manager did not see.
            if let Some(started) = self.slot(&timer.unit) {
                let target = &mut self.units[started];
                target.started_at = target.started_at.or(record.unit_started.map(from_millis));
                target.stopped_at = target.stopped_at.or(record.unit_stopped.map(from_millis));
            }
            let unit = &mut self.units[slot];
            unit.machine.adopt(false, Instant::now());
            unit.schedule.adopt(
                &timer,
                &name,
                from_millis(record.activated),
                record.last_trigger.map(from_millis),
                record.running,
                random(),
            );
            info!("{name}: active, as it was");
        }
        for (name, rest) in saved.rests {
            self.restore_rest(&name, rest);
        }
        self.dirty = true;
    }

    /// Leave a unit where the last manager left it: stopped, finished,
    /// failed or spent, it stays so; waiting to restart, it is started.
    fn restore_rest(&mut self, name: &str, rest: SavedRest) {
        let Some(slot) = self.slot(name) else {
            return;
        };
        let unit = &mut self.units[slot];
        // Its processes ran after all, and it was adopted.
        if unit.machine.state() != State::Inactive {
            return;
        }
        let failed = match rest.state {
            RestState::Inactive => false,
            RestState::Failed => true,
            // The delay was the last manager's; this one starts it now,
            // through the plan, like any start.
            RestState::AutoRestart => {
                info!("{name}: was waiting to restart; starting it");
                self.to_start.insert(name.to_owned());
                return;
            }
        };
        let outcome = rest.outcome.map(Outcome::from);
        unit.machine.rest(failed, outcome);
        unit.left_at_rest = true;
        if unit.machine.service().is_timer() {
            unit.schedule.last_trigger = rest.last_trigger.map(from_millis);
        }
        let line = match (failed, outcome) {
            (true, Some(outcome)) => format!("failed, as it was: it {outcome}"),
            (true, None) => "failed, as it was".to_owned(),
            (false, _) => "inactive, as it was".to_owned(),
        };
        info!("{name}: {line}");
    }

    // ---- the loop -------------------------------------------------------

    fn run(&mut self) {
        loop {
            self.read_controls();
            self.plan_step();
            self.forget_removed();
            match self.exit {
                Some(Exit::Detach) => break,
                Some(Exit::StopAll)
                    if self.to_stop.is_empty()
                        && self.units.iter().all(|u| u.resting() && u.main.is_none()) =>
                {
                    break
                }
                _ => {}
            }
            self.save_if_dirty();
            let timeout = self.timeout();
            match self.port.wait(Some(timeout)) {
                Ok(Some(packet)) => self.packet(packet),
                Ok(None) => {}
                Err(e) => {
                    error!("waiting on the completion port failed: {e}");
                    std::thread::sleep(TICK);
                }
            }
            self.deadlines();
            self.poll_jobs();
            self.check_shell();
            self.timers();
            self.check_logs();
        }
        match self.exit {
            Some(Exit::Detach) => {
                self.dirty = true;
                self.save_if_dirty();
                let running = self.units.iter().filter(|u| u.job.is_some()).count();
                info!("detached; {running} service(s) left running for the next manager")
            }
            _ => {
                // Everything was stopped, as at sign-out: the next manager
                // starts afresh, as at sign-in.
                if let Some(path) = self.state_path() {
                    if let Err(e) = state::remove(&path) {
                        error!("cannot remove the state file {}: {e}", path.display());
                    }
                }
                info!("stopped")
            }
        }
    }

    fn timeout(&self) -> Duration {
        let now = Instant::now();
        let mut timeout = if self.graphical { TICK } else { SHELL_POLL };
        let deadlines = self.units.iter().map(|u| u.machine.deadline());
        let tray = (!self.tray).then_some(self.tray_deadline).flatten();
        for deadline in deadlines.chain([tray]).flatten() {
            timeout = timeout.min(deadline.saturating_duration_since(now));
        }
        if let Some(next) = self.next_elapse {
            let left = next.duration_since(SystemTime::now()).unwrap_or_default();
            timeout = timeout.min(left);
        }
        timeout
    }

    fn read_controls(&mut self) {
        while let Ok(control) = self.inbox.try_recv() {
            match control {
                Control::Note(note) => info!("{note}"),
                Control::Request(request, reply) => {
                    let response = self.answer(request);
                    let _ = reply.send(response);
                }
                Control::Detach(reason) => {
                    if self.exit == Some(Exit::StopAll) {
                        info!("{reason}; already stopping everything, which continues");
                    } else {
                        info!("{reason}: detaching");
                        self.exit = Some(Exit::Detach);
                    }
                }
                Control::StopAll(reason) => {
                    if self.exit.is_none() {
                        info!("{reason}: stopping every service");
                        self.exit = Some(Exit::StopAll);
                        self.rests_before_stop_all = Some(self.rests());
                        self.to_start.clear();
                        self.to_stop = self
                            .units
                            .iter()
                            .filter(|u| !u.resting() || u.machine.state() == State::AutoRestart)
                            .map(|u| u.name.clone())
                            .collect();
                    } else if self.exit == Some(Exit::StopAll) {
                        info!("{reason} again: detaching instead of waiting");
                        self.exit = Some(Exit::Detach);
                    }
                }
            }
        }
    }

    fn reached(&self, target: &str) -> bool {
        match target {
            DEFAULT_TARGET | TIMERS_TARGET => true,
            GRAPHICAL_TARGET => self.graphical,
            TRAY_TARGET => self.tray,
            _ => false,
        }
    }

    fn plan_step(&mut self) {
        if self.exit == Some(Exit::StopAll) {
            loop {
                let ready = self.plan.ready_to_stop(&self.to_stop, |n| self.progress(n));
                if ready.is_empty() {
                    break;
                }
                for name in ready {
                    self.to_stop.remove(&name);
                    if let Some(slot) = self.slot(&name) {
                        self.feed(slot, UnitEvent::Stop);
                    }
                }
            }
            return;
        }
        loop {
            let decisions =
                self.plan
                    .ready_to_start(&self.to_start, |n| self.progress(n), |t| self.reached(t));
            if decisions.is_empty() {
                break;
            }
            for (name, decision) in decisions {
                self.to_start.remove(&name);
                let Some(slot) = self.slot(&name) else {
                    continue;
                };
                match decision {
                    Decision::Start => self.feed(slot, UnitEvent::Start),
                    Decision::StartBreakingCycle => {
                        warning!("{name}: its ordering is a cycle; starting it anyway");
                        self.feed(slot, UnitEvent::Start);
                    }
                    Decision::Fail { missing } => {
                        error!("{name}: not started: it requires {missing}, which failed or does not exist");
                        self.units[slot].machine.refuse(Outcome::Dependency);
                        self.dirty = true;
                    }
                }
            }
        }
    }

    /// Listen for Explorer's `TaskbarCreated` from before its taskbar exists,
    /// if it does not yet, so the broadcast that readies the tray is heard.
    fn watch_taskbar_created(&mut self) {
        match sys::shell::watch_taskbar_created(self.port.waker(), KEY_TASKBAR_CREATED) {
            Ok(()) => self.hears_taskbar_created = !sys::shell::taskbar_exists(),
            Err(e) => warning!(
                "cannot listen for Explorer's TaskbarCreated ({e}); \
                 {TRAY_TARGET} will be reached {TRAY_GRACE:?} after Explorer starts"
            ),
        }
    }

    fn check_shell(&mut self) {
        if !self.graphical && sys::shell::taskbar_exists() {
            self.reach_graphical();
        }
        if !self.tray && self.tray_deadline.is_some_and(|d| Instant::now() >= d) {
            if self.hears_taskbar_created {
                warning!(
                    "Explorer has not said its taskbar is ready, {TRAY_GRACE:?} after it \
                     started; {TRAY_TARGET} reached anyway"
                );
            } else {
                info!("Explorer has been running for {TRAY_GRACE:?} or more: the tray is ready");
            }
            self.reach_tray();
        }
    }

    fn taskbar_created(&mut self) {
        if self.tray {
            // Tray programs hear the same broadcast and add their icons again.
            info!("Explorer's taskbar was created again: Explorer restarted");
            return;
        }
        info!("Explorer says its taskbar is ready");
        if !self.graphical {
            self.reach_graphical();
        }
        self.reach_tray();
    }

    fn reach_graphical(&mut self) {
        self.graphical = true;
        info!("the shell is ready: {GRAPHICAL_TARGET} reached");
        // Timed from Explorer's start rather than from now: a manager that
        // finds a taskbar Explorer made long ago finds its tray ready too.
        let age = sys::shell::explorer_age().unwrap_or_default();
        self.tray_deadline = Some(Instant::now() + TRAY_GRACE.saturating_sub(age));
        if self.exit.is_none() {
            let wanted = self.plan.pulled_in_by(GRAPHICAL_TARGET);
            self.want_started(wanted);
        }
    }

    fn reach_tray(&mut self) {
        self.tray = true;
        info!("the tray takes icons: {TRAY_TARGET} reached");
        if self.exit.is_none() {
            let wanted = self.plan.pulled_in_by(TRAY_TARGET);
            self.want_started(wanted);
        }
    }

    fn deadlines(&mut self) {
        let now = Instant::now();
        for slot in 0..self.units.len() {
            let unit = &self.units[slot];
            if !unit.machine.deadline().is_some_and(|d| now >= d) {
                continue;
            }
            // Nothing comes back while everything is being stopped.
            if self.exit.is_some() && unit.machine.state() == State::AutoRestart {
                continue;
            }
            self.feed(slot, UnitEvent::Deadline);
        }
    }

    // ---- timers ---------------------------------------------------------

    /// What a timer's relative triggers count from, the unit it starts
    /// included.
    fn moments(&self, slot: usize) -> Moments {
        let started = self.units[slot]
            .machine
            .service()
            .timer
            .as_ref()
            .and_then(|t| self.slot(&t.unit))
            .map(|s| &self.units[s]);
        Moments {
            boot: self.boot,
            startup: self.startup,
            unit_started: started.and_then(|u| u.started_at),
            unit_stopped: started.and_then(|u| u.stopped_at),
        }
    }

    /// When a timer is next due, if it is running and anything is.
    fn next_elapse_of(&self, slot: usize, now: SystemTime, zone: &Local) -> Option<SystemTime> {
        let unit = &self.units[slot];
        let timer = unit.machine.service().timer.as_ref()?;
        unit.schedule.next(timer, &self.moments(slot), now, zone)
    }

    /// Start what the timers that are due start. A timer that has elapsed
    /// may elapse again once what it started is at rest; one with nothing
    /// more to wait for and `RemainAfterElapse=no` stops.
    fn timers(&mut self) {
        self.next_elapse = None;
        // Nothing starts while everything is being stopped.
        if self.exit.is_some() {
            return;
        }
        let zone = Local::current();
        let now = SystemTime::now();
        for slot in 0..self.units.len() {
            let unit = &self.units[slot];
            let Some(timer) = unit.machine.service().timer.clone() else {
                continue;
            };
            if unit.machine.state() != State::Active {
                continue;
            }
            if unit.schedule.running {
                let at_rest = self.slot(&timer.unit).is_none_or(|s| {
                    matches!(
                        self.units[s].machine.state(),
                        State::Inactive | State::Failed
                    ) && !self.to_start.contains(&timer.unit)
                });
                if at_rest {
                    self.units[slot].schedule.unit_at_rest();
                }
            }
            let next = self.next_elapse_of(slot, now, &zone);
            match next {
                Some(due) if due <= now => self.elapse(slot, now),
                Some(due) => {
                    self.next_elapse = Some(self.next_elapse.map_or(due, |n| n.min(due)));
                }
                None if self.units[slot].schedule.is_done(&timer, None) => {
                    self.mark(slot, "nothing more is due");
                    self.feed(slot, UnitEvent::Stop);
                }
                None => {}
            }
        }
    }

    /// A timer is due: start what it starts.
    fn elapse(&mut self, slot: usize, now: SystemTime) {
        let name = self.units[slot].name.clone();
        let Some(timer) = self.units[slot].machine.service().timer.clone() else {
            return;
        };
        self.units[slot].schedule.fire(&timer, &name, now, random());
        self.dirty = true;
        if timer.persistent {
            if let Some(dir) = &self.state_dir {
                if let Err(e) = state::write_stamp(dir, &name, now) {
                    warning!("{name}: cannot record when it elapsed: {e}");
                }
            }
        }
        let started = &timer.unit;
        let line = match self.slot(started).filter(|&s| !self.units[s].removed) {
            None => format!("elapsed, but {started} does not exist"),
            Some(s) if !self.units[s].resting() => {
                format!("elapsed; {started} is still running")
            }
            Some(_) => format!("elapsed; starting {started}"),
        };
        info!("{name}: {line}");
        self.mark(slot, &line);
        self.start_with_dependencies(started);
    }

    /// Notice empty jobs without relying on the job's notifications, which
    /// Windows does not promise to deliver.
    fn poll_jobs(&mut self) {
        for slot in 0..self.units.len() {
            let unit = &self.units[slot];
            let empty = match &unit.job {
                Some(job) if !unit.job_empty_fed => job.active_processes().is_ok_and(|n| n == 0),
                _ => false,
            };
            if empty {
                self.feed(slot, UnitEvent::JobEmpty);
            }
        }
    }

    fn packet(&mut self, packet: Packet) {
        match packet.key {
            KEY_WAKE => {}
            KEY_EXIT => self.process_exited(packet.value),
            KEY_TASKBAR_CREATED => self.taskbar_created(),
            key if key >= KEY_JOB_BASE => {
                self.job_message(key - KEY_JOB_BASE, packet.bytes, packet.value as u32)
            }
            _ => {}
        }
    }

    fn process_exited(&mut self, token: usize) {
        // A shim's exit is not a unit's: it is not the main or the control
        // process, and the machine never hears of it.
        if let Some(slot) = self.units.iter().position(
            |u| matches!(&u.output, Some(Output::EventLog { shim, .. }) if shim.token == token),
        ) {
            self.shim_exited(slot);
            return;
        }
        let found = self.units.iter().enumerate().find_map(|(slot, u)| {
            if u.main.as_ref().is_some_and(|t| t.token == token) {
                Some((slot, Process::Main))
            } else if u.control.as_ref().is_some_and(|t| t.token == token) {
                Some((slot, Process::Control))
            } else {
                None
            }
        });
        let Some((slot, which)) = found else { return };
        let unit = &mut self.units[slot];
        let tracked = match which {
            Process::Main => unit.main.take(),
            Process::Control => unit.control.take(),
        }
        .expect("found above");
        let code = tracked.child.exit_code().unwrap_or(STATUS_UNSUCCESSFUL);
        let outcome = steward_supervisor::policy::classify(code);
        let kind = if which == Process::Main {
            "main"
        } else {
            "control"
        };
        let message = if code == KILLED {
            format!("{kind} process {} was terminated", tracked.child.pid)
        } else {
            format!(
                "{kind} process {} {outcome} (0x{code:X})",
                tracked.child.pid
            )
        };
        drop(tracked);
        self.mark(slot, &message);
        if which == Process::Main {
            self.dirty = true;
        }
        let empty = self.units[slot].job.as_ref().is_none_or(settles_empty);
        if empty {
            self.feed(slot, UnitEvent::JobEmpty);
        }
        self.feed(slot, UnitEvent::Exited(which, code));
    }

    fn job_message(&mut self, serial: usize, message: u32, pid: u32) {
        let Some(slot) = self
            .units
            .iter()
            .position(|u| u.job.is_some() && u.job_serial == serial)
        else {
            return;
        };
        match message {
            JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO => {
                if !self.units[slot].job_empty_fed {
                    self.feed(slot, UnitEvent::JobEmpty);
                }
            }
            // Any NTSTATUS error: a crash, or the exit of a Ctrl+C.
            JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS => {
                let code = process::exit_code_of(pid).unwrap_or(STATUS_UNSUCCESSFUL);
                let outcome = steward_supervisor::policy::classify(code);
                self.mark(slot, &format!("process {pid} {outcome}"));
                self.feed(slot, UnitEvent::Crashed(code));
                self.dirty = true;
            }
            // The job's membership changed: what a restarted manager would
            // need to adopt has too.
            JOB_OBJECT_MSG_NEW_PROCESS | JOB_OBJECT_MSG_EXIT_PROCESS => self.dirty = true,
            _ => {}
        }
    }

    // ---- feeding the machines and carrying out their actions -------------

    fn feed(&mut self, slot: usize, event: UnitEvent) {
        if event == UnitEvent::JobEmpty {
            self.units[slot].job_empty_fed = true;
        }
        let before = self.units[slot].machine.state();
        let actions = self.units[slot].machine.handle(event, Instant::now());
        let after = self.units[slot].machine.state();
        // A target's or a timer's state is all a next manager has to go on.
        if self.units[slot].machine.service().runs_nothing() && after != before {
            self.dirty = true;
        }
        if let Some(timer) = self.units[slot].machine.service().timer.clone() {
            let unit = &mut self.units[slot];
            if after == State::Active && before != State::Active {
                let stamp = self
                    .state_dir
                    .as_ref()
                    .filter(|_| timer.persistent)
                    .and_then(|dir| state::read_stamp(dir, &unit.name));
                unit.schedule
                    .start(&timer, &unit.name, SystemTime::now(), stamp, random());
            } else if after != State::Active && before == State::Active {
                unit.schedule.stop();
            }
        }
        // Before the actions: their replies report the transitions they cause.
        self.report(slot, before);
        self.carry_out(slot, actions);
        self.release_job(slot);
    }

    fn carry_out(&mut self, slot: usize, actions: Vec<Action>) {
        let mut replies = VecDeque::new();
        for action in actions {
            if let Some(reply) = self.execute(slot, action) {
                replies.push_back(reply);
            }
        }
        for reply in replies {
            self.feed(slot, reply);
        }
    }

    fn execute(&mut self, slot: usize, action: Action) -> Option<UnitEvent> {
        let name = self.units[slot].name.clone();
        match action {
            Action::SpawnMain(command) => Some(self.spawn(slot, &command, Process::Main)),
            Action::SpawnControl(command) => Some(self.spawn(slot, &command, Process::Control)),
            Action::AskToExit => {
                if let Some(job) = &self.units[slot].job {
                    match job.pids() {
                        Ok(pids) if !pids.is_empty() => {
                            self.mark(slot, &format!("asking {pids:?} to exit"));
                            if let Err(e) = signal::ask_to_exit(&pids) {
                                warning!("{name}: cannot run the Ctrl+C helper: {e}");
                            }
                        }
                        Ok(_) => {}
                        Err(e) => warning!("{name}: cannot list its processes: {e}"),
                    }
                }
                None
            }
            Action::Kill => {
                if let Some(job) = &self.units[slot].job {
                    self.mark(slot, "terminating every process in its job");
                    if let Err(e) = job.terminate(KILLED) {
                        error!("{name}: cannot terminate its job: {e}");
                    }
                }
                None
            }
            Action::KillControl => {
                if let Some(control) = &self.units[slot].control {
                    if let Err(e) = control.child.terminate(KILLED) {
                        warning!("{name}: cannot terminate its control process: {e}");
                    }
                }
                None
            }
        }
    }

    fn spawn(&mut self, slot: usize, command: &Command, which: Process) -> UnitEvent {
        match self.try_spawn(slot, command, which) {
            Ok(pid) => {
                let kind = if which == Process::Main {
                    "main"
                } else {
                    "control"
                };
                self.mark(
                    slot,
                    &format!("started {kind} process {pid}: {}", command.line),
                );
                UnitEvent::Spawned(which)
            }
            Err(e) => {
                let name = &self.units[slot].name;
                error!("{name}: cannot start {}: {e}", command.line);
                self.mark(slot, &format!("cannot start {}: {e}", command.line));
                UnitEvent::SpawnFailed(which)
            }
        }
    }

    fn try_spawn(
        &mut self,
        slot: usize,
        command: &Command,
        which: Process,
    ) -> std::io::Result<u32> {
        let fresh = self.units[slot].job.is_none();
        if fresh {
            self.new_job(slot)?;
        }
        let service = self.units[slot].machine.service().clone();
        // A run's output is decided at its first spawn -- or, for a unit
        // adopted from another manager, at this manager's first: the pipes
        // and the shim were the other's and went with it, so what this one
        // starts for the unit gets a shim of its own.
        if service.standard_output == steward_unit::Output::EventLog
            && self.units[slot].output.is_none()
        {
            self.open_channel(slot);
        }
        let to_channel = matches!(self.units[slot].output, Some(Output::EventLog { .. }));
        if fresh && !to_channel {
            self.rotate_log(slot);
        }
        let vars = env::merge(env::user_environment()?, &service.environment);
        let directory = service
            .working_directory
            .clone()
            .or_else(|| env::get(&vars, "USERPROFILE").map(str::to_owned))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let stdin = self
            .stdin
            .as_ref()
            .ok_or_else(|| std::io::Error::other("no NUL handle"))?;
        let unit = &self.units[slot];
        let job = unit.job.as_ref().expect("created above");
        let log;
        let (stdout, stderr) = match &unit.output {
            Some(Output::EventLog { stdout, stderr, .. }) => (stdout, stderr),
            _ => {
                log = process::open_log(&self.log_path(slot).ok_or_else(no_state_dir)?)?;
                (&log, &log)
            }
        };
        let child = process::spawn(
            &command.line,
            Some(&env::block(&vars)),
            Some(&directory),
            Some(job),
            &Handles {
                stdin,
                stdout,
                stderr,
                also: &[],
            },
            true,
        )?;
        let pid = child.pid;
        let tracked = match self.track(child) {
            Ok(tracked) => tracked,
            Err((child, e)) => {
                let _ = child.terminate(KILLED);
                return Err(e);
            }
        };
        let unit = &mut self.units[slot];
        unit.job_empty_fed = false;
        match which {
            Process::Main => {
                unit.main = Some(tracked);
                self.dirty = true;
            }
            Process::Control => unit.control = Some(tracked),
        }
        Ok(pid)
    }

    fn track(&mut self, child: Child) -> Result<Tracked, (Child, std::io::Error)> {
        let token = self.next_token;
        self.next_token += 1;
        match ExitWatch::new(&child, self.port.waker(), KEY_EXIT, token) {
            Ok(watch) => Ok(Tracked {
                child,
                token,
                _watch: watch,
            }),
            Err(e) => Err((child, e)),
        }
    }

    /// A `StandardOutput=eventlog` unit's run begins: its pipes and its
    /// `steward-cat`, or -- if the shim cannot be started -- its file, with
    /// a word about why in both logs and in `stewctl status`.
    fn open_channel(&mut self, slot: usize) {
        let name = self.units[slot].name.clone();
        match self.start_shim(&name) {
            Ok(output) => {
                if let Output::EventLog { shim, .. } = &output {
                    info!(
                        "{name}: steward-cat {} carries its output to the Event Log",
                        shim.child.pid
                    );
                }
                self.units[slot].output = Some(output);
            }
            Err(e) => {
                warning!(
                    "{name}: cannot start steward-cat ({e}); its output goes to its log file \
                     for this run"
                );
                self.units[slot].output = Some(Output::Fallback(e.to_string()));
                self.dirty = true;
                self.mark(
                    slot,
                    &format!(
                        "steward-cat could not be started ({e}); output goes to this file \
                         instead of the Event Log for this run"
                    ),
                );
            }
        }
    }

    /// Two pipes and a `steward-cat` reading them: in no job (see
    /// [`Output::EventLog`]), with no console, its own few words going to
    /// `steward.log`. It is given the read ends by number, and only those:
    /// the write ends are for the unit's processes, and a copy of one in
    /// the shim would keep it from ever seeing the end of the output.
    fn start_shim(&mut self, name: &str) -> std::io::Result<Output> {
        let exe = std::env::current_exe()?.with_file_name("steward-cat.exe");
        if !exe.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} does not exist", exe.display()),
            ));
        }
        let (out_read, out_write) = process::pipe()?;
        let (err_read, err_write) = process::pipe()?;
        let stdin = self
            .stdin
            .as_ref()
            .ok_or_else(|| std::io::Error::other("no NUL handle"))?;
        let own_log = process::open_log(&crate::log::path().ok_or_else(no_state_dir)?)?;
        let line = format!(
            "{} {} {:#x} {:#x}",
            quote(&exe.to_string_lossy()),
            quote(name),
            out_read.as_raw_handle() as usize,
            err_read.as_raw_handle() as usize
        );
        let child = process::spawn(
            &line,
            None,
            None,
            None,
            &Handles {
                stdin,
                stdout: &own_log,
                stderr: &own_log,
                also: &[&out_read, &err_read],
            },
            false,
        )?;
        // The shim has its copies of the read ends; the manager's close
        // here, so that a shim that dies leaves the unit's writes failing
        // rather than blocking on a pipe nobody drains.
        drop((out_read, err_read));
        let shim = match self.track(child) {
            Ok(tracked) => tracked,
            Err((child, e)) => {
                let _ = child.terminate(KILLED);
                return Err(e);
            }
        };
        Ok(Output::EventLog {
            stdout: out_write,
            stderr: err_write,
            shim,
        })
    }

    /// The unit's `steward-cat` exited while the manager still held a write
    /// end of its pipes, so not for the end of the output: it crashed, or
    /// something ended it. The unit's running processes now write into a
    /// pipe nobody reads, which is the one way an eventlog unit loses its
    /// output, and there is nothing to be done for them; what the manager
    /// starts for the unit from here on writes to the file instead.
    fn shim_exited(&mut self, slot: usize) {
        let Some(Output::EventLog { shim, .. }) = self.units[slot].output.take() else {
            return;
        };
        let code = shim.child.exit_code().unwrap_or(STATUS_UNSUCCESSFUL);
        let why = format!(
            "steward-cat {} exited (0x{code:X}) with the unit still running",
            shim.child.pid
        );
        drop(shim);
        let name = self.units[slot].name.clone();
        error!("{name}: {why}; its output from here on goes to its log file");
        self.units[slot].output = Some(Output::Fallback(why.clone()));
        self.dirty = true;
        self.mark(
            slot,
            &format!(
                "{why}: what its running processes write from now on is lost, and what \
                 steward starts for it next writes to this file"
            ),
        );
    }

    /// Every start (and every adoption) gets a fresh job.
    fn new_job(&mut self, slot: usize) -> std::io::Result<()> {
        let serial = self.next_serial;
        self.next_serial += 1;
        let unit = &self.units[slot];
        let children_break_away = unit.machine.service().kill_mode == KillMode::Process;
        let job = Job::create(children_break_away)?;
        if let Err(e) = job.notify(&self.port, KEY_JOB_BASE + serial) {
            warning!(
                "{}: its job's notifications cannot be had ({e}); watching it by polling",
                unit.name
            );
        }
        let unit = &mut self.units[slot];
        unit.job = Some(job);
        unit.job_serial = serial;
        self.dirty = true;
        Ok(())
    }

    /// A unit at rest whose job is empty lets go of it, and of the run's
    /// output with it: the write ends close, and the shim, once the unit's
    /// own copies have closed too, reads to the end and exits.
    fn release_job(&mut self, slot: usize) {
        let unit = &mut self.units[slot];
        if unit.job.is_some()
            && unit.resting()
            && unit.main.is_none()
            && unit.control.is_none()
            && unit.job_empty_fed
        {
            unit.job = None;
            self.dirty = true;
        }
        if unit.job.is_none() && unit.output.is_some() {
            unit.output = None;
            self.dirty = true;
        }
    }

    fn report(&mut self, slot: usize, before: State) {
        let unit = &self.units[slot];
        let after = unit.machine.state();
        if after == before {
            return;
        }
        let at_rest = |s: State| matches!(s, State::Inactive | State::Failed);
        let unit = &mut self.units[slot];
        unit.since = (Instant::now(), crate::log::timestamp());
        if at_rest(before) != at_rest(after) {
            let now = Some(SystemTime::now());
            if at_rest(after) {
                unit.stopped_at = now;
            } else {
                unit.started_at = now;
                unit.left_at_rest = false;
            }
            // A timer that starts it counts from these.
            self.dirty = true;
        }
        let unit = &self.units[slot];
        let name = unit.name.clone();
        let last = unit.machine.last_outcome();
        let line = match after {
            State::Active if unit.machine.service().is_timer() => {
                let zone = Local::current();
                match self.next_elapse_of(slot, SystemTime::now(), &zone) {
                    Some(next) => format!("active; next elapse {}", clock::format(next, &zone)),
                    None => "active".to_owned(),
                }
            }
            State::Active => {
                let pid = unit.main.as_ref().map(|t| t.child.pid);
                pid.map_or("active".to_owned(), |pid| {
                    format!("active, main process {pid}")
                })
            }
            State::AutoRestart => {
                let delay = unit.machine.deadline().map_or(0.0, |d| {
                    d.saturating_duration_since(Instant::now()).as_secs_f64()
                });
                format!(
                    "{}; restarting in {delay:.1} s (restart {})",
                    last.map_or("ended".to_owned(), |o| o.to_string()),
                    unit.machine.restarts()
                )
            }
            State::Failed => format!(
                "failed: it {}",
                last.map_or("failed".to_owned(), |o| o.to_string())
            ),
            State::Inactive => match last {
                Some(Outcome::Clean) | None => "inactive".to_owned(),
                Some(outcome) => format!("inactive; it {outcome}"),
            },
            other => other.name().to_owned(),
        };
        if after == State::Failed {
            error!("{name}: {line}");
            self.mark_at(slot, Level::Error, &line);
        } else {
            info!("{name}: {line}");
            self.mark(slot, &line);
        }
    }

    // ---- files ----------------------------------------------------------

    fn state_path(&self) -> Option<PathBuf> {
        Some(state::path(self.state_dir.as_ref()?, self.session))
    }

    fn log_path(&self, slot: usize) -> Option<PathBuf> {
        let dir = self.state_dir.as_ref()?.join("logs");
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir.join(format!("{}.log", self.units[slot].name)))
    }

    /// Every unit's log, measured every [`LOG_CHECK`]: a start is not the
    /// only time a log can pass the cap.
    fn check_logs(&mut self) {
        if self.logs_checked.elapsed() < LOG_CHECK {
            return;
        }
        self.logs_checked = Instant::now();
        for slot in 0..self.units.len() {
            self.rotate_log(slot);
        }
    }

    /// A unit's log over [`crate::log::CAP`] is set aside as `<unit>.log.1`
    /// and begun again in place (see [`crate::log::set_aside`]): its
    /// processes hold an inherited handle to it, and would go on writing to
    /// a renamed file. A line near the top of the new log says so (its
    /// processes may get a line in first) -- and, while something runs, that
    /// a line written during the copy may be missing; at a start there is no
    /// writer, and nothing is lost.
    fn rotate_log(&mut self, slot: usize) {
        // A unit writing to the Event Log has no file growing; one that has
        // fallen back to its file has, and is measured like any other.
        if matches!(self.units[slot].output, Some(Output::EventLog { .. })) {
            return;
        }
        let Some(path) = self.log_path(slot) else {
            return;
        };
        if !crate::log::size(&path).is_ok_and(|size| size > crate::log::CAP) {
            return;
        }
        let unit = &self.units[slot];
        let running = unit.job.is_some() || unit.main.is_some() || unit.control.is_some();
        let aside = crate::log::aside(&path);
        match crate::log::set_aside(&path) {
            Ok(bytes) => {
                let name = aside.file_name().unwrap_or_default().to_string_lossy();
                let lost = if running {
                    "; a line written while it was copied may be missing"
                } else {
                    ""
                };
                self.units[slot].log_warned = false;
                self.mark(
                    slot,
                    &format!(
                        "the log passed {} MiB: its first {:.1} MiB are set aside as {name}{lost}",
                        crate::log::CAP >> 20,
                        bytes as f64 / (1u64 << 20) as f64
                    ),
                );
            }
            Err(e) if !unit.log_warned => {
                warning!(
                    "{}: its log is over {} MiB and cannot be set aside as {}: {e}",
                    unit.name,
                    crate::log::CAP >> 20,
                    aside.display()
                );
                self.units[slot].log_warned = true;
            }
            Err(_) => {}
        }
    }

    /// A line from steward itself in the unit's log, between its output: in
    /// its file or, for a `StandardOutput=eventlog` unit, in the channel, as
    /// an event of the `steward` stream from the manager's own registration
    /// of the user's provider.
    fn mark(&self, slot: usize, message: &str) {
        self.mark_at(slot, Level::Info, message);
    }

    fn mark_at(&self, slot: usize, level: Level, message: &str) {
        let unit = &self.units[slot];
        let to_channel = unit.machine.service().standard_output == steward_unit::Output::EventLog
            && !matches!(unit.output, Some(Output::Fallback(_)));
        if to_channel {
            if !self.mark_event(&unit.name, level, message) {
                // Nobody listens -- the channel does not exist yet, or the
                // Event Log service is restarting -- so the line goes here,
                // beside the line saying that nobody does, which is what a
                // reader wondering where the unit's lines went is after.
                info!("{}: {message}", unit.name);
            }
            return;
        }
        let Some(path) = self.log_path(slot) else {
            return;
        };
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            // One write: the unit's processes append to the same file, and
            // a line written piecemeal has their output in the middle of it.
            let line = format!("-- {} steward: {message}\n", crate::log::timestamp());
            let _ = file.write_all(line.as_bytes());
        }
    }

    /// Writes a mark as an event, if a session is listening for them. Says
    /// once when none is, and once more each time that comes back.
    fn mark_event(&self, unit: &str, level: Level, message: &str) -> bool {
        let provider = self.provider.get_or_init(|| {
            let sid = match steward_ipc::pipe::user_sid() {
                Ok(sid) => sid,
                Err(e) => {
                    error!("cannot tell the user's SID, so no Event Log provider: {e}");
                    return None;
                }
            };
            let name = provider_name(&sid);
            let channel = Channel {
                guid: provider_guid(&sid).to_u128(),
                name: &name,
                channel: CHANNEL_VALUE,
                keyword: CHANNEL_KEYWORD,
            };
            match Provider::register(&channel, None) {
                Ok(provider) => {
                    info!("registered {name}, to write to {}", channel_name(&sid));
                    Some(provider)
                }
                Err(e) => {
                    error!(
                        "cannot register the Event Log provider {name} ({e}); steward's lines \
                         about eventlog units go here"
                    );
                    None
                }
            }
        });
        let Some(provider) = provider else {
            return false;
        };
        if !provider.listening() {
            if !self.channel_quiet.replace(true) {
                warning!(
                    "no session listens to the Event Log provider: the channel has not been \
                     created yet, or the Event Log service is restarting; steward's lines about \
                     eventlog units go here until one does"
                );
            }
            return false;
        }
        if self.channel_quiet.replace(false) {
            info!("a session listens to the Event Log provider now");
        }
        provider.output_at(
            level,
            &utf16::cstr(unit),
            Stream::Steward,
            &utf16::cstr(message),
        )
    }

    fn save_if_dirty(&mut self) {
        if !std::mem::take(&mut self.dirty) {
            return;
        }
        let Some(path) = self.state_path() else {
            return;
        };
        let mut saved = Saved {
            logon: self.logon.map(to_millis),
            rests: match &self.rests_before_stop_all {
                Some(rests) => rests.clone(),
                None => self.rests(),
            },
            ..Saved::default()
        };
        for unit in &self.units {
            let Some(job) = &unit.job else { continue };
            let processes: Vec<SavedProcess> = job
                .pids()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|pid| {
                    let created = process::creation_time_of(pid)?;
                    Some(SavedProcess { pid, created })
                })
                .collect();
            if processes.is_empty() {
                continue;
            }
            let main = unit.main.as_ref().map(|t| SavedProcess {
                pid: t.child.pid,
                created: t.child.created,
            });
            let output_fallback = match &unit.output {
                Some(Output::Fallback(why)) => Some(why.clone()),
                _ => None,
            };
            saved.units.insert(
                unit.name.clone(),
                SavedUnit {
                    main,
                    processes,
                    output_fallback,
                },
            );
        }
        saved.targets = self
            .units
            .iter()
            .filter(|u| u.machine.service().is_target() && u.machine.state() == State::Active)
            .map(|u| u.name.clone())
            .collect();
        for (slot, unit) in self.units.iter().enumerate() {
            let Some(activated) = unit.schedule.activated else {
                continue;
            };
            if unit.machine.state() != State::Active {
                continue;
            }
            let moments = self.moments(slot);
            saved.timers.insert(
                unit.name.clone(),
                SavedTimer {
                    activated: to_millis(activated),
                    last_trigger: unit.schedule.last_trigger.map(to_millis),
                    running: unit.schedule.running,
                    unit_started: moments.unit_started.map(to_millis),
                    unit_stopped: moments.unit_stopped.map(to_millis),
                },
            );
        }
        if let Err(e) = state::save(&path, &saved) {
            error!("cannot save the state to {}: {e}", path.display());
        }
    }

    /// The units at rest a next manager is to leave there (or restart).
    fn rests(&self) -> BTreeMap<String, SavedRest> {
        self.units
            .iter()
            .filter_map(|u| Some((u.name.clone(), u.rest()?)))
            .collect()
    }
}

/// Every unit in the unit directory that loads, and a line for each problem.
fn read_units() -> Result<(Vec<Service>, Vec<String>), String> {
    let dir = steward_unit::user_unit_dir()
        .ok_or("APPDATA is not set; cannot find the unit directory")?;
    let loaded =
        steward_unit::load_dir(&dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let mut services = Vec::new();
    let mut messages = Vec::new();
    for unit in loaded {
        let name = unit
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        for diagnostic in &unit.parsed.diagnostics {
            messages.push(format!("{name}: {diagnostic}"));
        }
        match unit.parsed.service {
            Some(service) => services.push(service),
            None => messages.push(format!("{name}: not loaded")),
        }
    }
    info!("{} unit(s) in {}", services.len(), dir.display());
    Ok((services, messages))
}

/// Whether the job is empty, or about to be: a console program's console
/// host lives in the job too, and outlasts the program by a moment.
fn settles_empty(job: &Job) -> bool {
    for _ in 0..20 {
        if job.active_processes().is_ok_and(|n| n == 0) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// A random number, for `RandomizedDelaySec=`: std's hasher keys are random.
fn random() -> u64 {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    hasher.finish()
}

fn no_state_dir() -> std::io::Error {
    std::io::Error::other("LOCALAPPDATA is not set; nowhere to put the unit's log")
}

/// `arg` as one argument of a Windows command line: as it is if nothing in
/// it would split it, otherwise quoted the way `CommandLineToArgvW` (and so
/// a Rust program's `args_os`) reads it back.
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '"']) {
        return arg.to_owned();
    }
    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        // Backslashes count only before a quote, where they are doubled
        // and the quote escaped.
        let run = if c == '"' {
            2 * backslashes + 1
        } else {
            backslashes
        };
        quoted.extend(std::iter::repeat_n('\\', run));
        quoted.push(c);
        backslashes = 0;
    }
    quoted.extend(std::iter::repeat_n('\\', 2 * backslashes));
    quoted.push('"');
    quoted
}

// ---- running in a console -------------------------------------------------

static CONSOLE: OnceLock<(Sender<Control>, Waker)> = OnceLock::new();

unsafe extern "system" fn on_console_ctrl(_ctrl_type: u32) -> BOOL {
    if let Some((controls, waker)) = CONSOLE.get() {
        let _ = controls.send(Control::StopAll("Ctrl+C".into()));
        let _ = waker.post(KEY_WAKE, 0, 0);
    }
    1
}

/// `steward --console`: the same manager, in the foreground. Ctrl+C stops
/// every service and exits; a second Ctrl+C exits leaving them running. The
/// exit code is 1 if the manager never ran.
pub fn run_console() {
    crate::log::init(true);
    let port = match Port::new() {
        Ok(port) => port,
        Err(e) => {
            error!("cannot create a completion port: {e}");
            std::process::exit(1);
        }
    };
    let (controls, inbox) = mpsc::channel();
    let _ = CONSOLE.set((controls.clone(), port.waker()));
    unsafe { SetConsoleCtrlHandler(Some(on_console_ctrl), 1) };
    if run(port, controls, inbox) != Ending::Stopped {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::quote;

    /// Each quoted form reads back as the argument: checked against what
    /// `CommandLineToArgvW` does, which `std::env::args` follows.
    #[test]
    fn arguments_are_quoted_as_windows_reads_them() {
        assert_eq!(quote("whkd.service"), "whkd.service");
        assert_eq!(
            quote(r"C:\steward\steward-cat.exe"),
            r"C:\steward\steward-cat.exe"
        );
        assert_eq!(
            quote(r"C:\Program Files\steward\steward-cat.exe"),
            r#""C:\Program Files\steward\steward-cat.exe""#
        );
        assert_eq!(quote("my unit.service"), r#""my unit.service""#);
        assert_eq!(quote(""), r#""""#);
        // A trailing backslash is doubled so that it does not escape the
        // closing quote; a quote is escaped, and the backslashes before it
        // doubled.
        assert_eq!(quote(r"a b\"), r#""a b\\""#);
        assert_eq!(quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(quote(r"a\b c"), r#""a\b c""#);
    }
}
