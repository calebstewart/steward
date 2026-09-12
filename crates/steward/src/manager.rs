//! The manager: one thread, waiting on one completion port, turning what
//! happens to processes and jobs into events for each service's state machine
//! and carrying out the actions the machines answer with.
//!
//! Starts and stops are ordered by the plan: `default.target` is reached at
//! once, `graphical-session.target` when the shell is ready. On the way out
//! the manager either stops everything, in reverse order (the SCM stopping
//! the instance, which is what sign-out does; system shutdown; Ctrl+C in a
//! console), or detaches, leaving its services running and their jobs
//! recorded for the next manager to adopt (handing over to a new manager, as
//! an upgrade does).

use std::collections::{BTreeSet, VecDeque};
use std::io::Write;
use std::os::windows::io::OwnedHandle;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use steward_ipc::{ManagerStatus, Request, Response, UnitStatus};
use steward_supervisor::plan::{DEFAULT_TARGET, GRAPHICAL_TARGET};
use steward_supervisor::{
    Action, Decision, Event as UnitEvent, Machine, Outcome, Plan, Process, Progress, State,
};
use steward_unit::{Command, KillMode, Service};
use windows_sys::core::BOOL;
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::SystemServices::{
    JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS, JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO,
    JOB_OBJECT_MSG_EXIT_PROCESS, JOB_OBJECT_MSG_NEW_PROCESS,
};

use crate::control;
use crate::log::{error, info, warning};
use crate::state::{self, Saved, SavedProcess, SavedUnit};
use crate::sys::job::Job;
use crate::sys::port::{Packet, Port, Waker};
use crate::sys::process::{self, Child, ExitWatch};
use crate::sys::{self, env, signal};

/// A nudge: there are controls in the channel.
pub const KEY_WAKE: usize = 1;
/// A process exited; the packet's value is its token.
const KEY_EXIT: usize = 2;
/// A job's notification; the key is this plus the job's serial number.
const KEY_JOB_BASE: usize = 0x1_0000;

/// The longest the manager waits before looking at the world again.
const TICK: Duration = Duration::from_secs(1);
/// How often to look for the shell until it is there.
const SHELL_POLL: Duration = Duration::from_millis(250);
/// A unit's log is set aside, once, when it passes this size at a start.
const LOG_ROTATE_BYTES: u64 = 8 << 20;
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
}

struct Manager {
    port: Port,
    inbox: Receiver<Control>,
    units: Vec<Unit>,
    plan: Plan,
    to_start: BTreeSet<String>,
    to_stop: BTreeSet<String>,
    graphical: bool,
    exit: Option<Exit>,
    next_token: usize,
    next_serial: usize,
    dirty: bool,
    state_dir: Option<PathBuf>,
    /// The session this manager belongs to, and its services with it.
    session: u32,
    stdin: Option<OwnedHandle>,
}

/// Run the manager until it is told to stop or detach. `controls` is the
/// sending end of `inbox`, for the control plane.
pub fn run(port: Port, controls: Sender<Control>, inbox: Receiver<Control>) {
    let session = sys::own_session();
    info!(
        "steward {} starting in session {session}",
        env!("CARGO_PKG_VERSION")
    );
    // The pipe is also the lock: one manager per session.
    if let Err(e) = control::listen(controls, port.waker()) {
        error!("cannot serve the control pipe: {e}; not starting");
        return;
    }
    let mut manager = Manager {
        port,
        inbox,
        units: Vec::new(),
        plan: Plan::default(),
        to_start: BTreeSet::new(),
        to_stop: BTreeSet::new(),
        graphical: false,
        exit: None,
        next_token: 1,
        next_serial: 1,
        dirty: false,
        state_dir: crate::log::state_dir(),
        session,
        stdin: process::open_null()
            .map_err(|e| error!("cannot open NUL for services' stdin: {e}"))
            .ok(),
    };
    manager.load_units();
    manager.adopt();
    let wanted = manager.plan.pulled_in_by(DEFAULT_TARGET);
    manager.want_started(wanted);
    manager.check_shell();
    manager.run();
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
            if self.progress(&name) == Progress::Idle {
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
                for (name, slot) in units.into_iter().zip(slots) {
                    self.to_start.remove(&name);
                    self.feed(slot, UnitEvent::Stop);
                    messages.push(format!("{name}: stopping"));
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
                // Skip the rest of the delay.
                State::AutoRestart => {
                    self.feed(slot, UnitEvent::Start);
                    messages.push(format!("{unit}: starting now instead of after its delay"));
                }
                _ if unit == name => messages.push(format!("{unit}: already running")),
                _ => {}
            }
        }
        messages
    }

    fn restart(&mut self, slot: usize) -> Vec<String> {
        let name = self.units[slot].name.clone();
        if self.units[slot].resting() {
            return self.start_with_dependencies(&name);
        }
        // A running unit restarts through its stop: the machine starts it
        // again once the stop is done, with its newest definition.
        self.feed(slot, UnitEvent::Stop);
        self.feed(slot, UnitEvent::Start);
        vec![format!("{name}: restarting")]
    }

    /// Read the unit files again; with `apply`, make what runs match them.
    fn reload(&mut self, apply: bool) -> Response {
        let (services, mut messages) = match read_units() {
            Ok(read) => read,
            Err(e) => return Response::error(e),
        };
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
        for (name, service) in std::mem::take(&mut fresh) {
            match self.slot(&name) {
                Some(slot) => {
                    let unit = &mut self.units[slot];
                    let came_back = std::mem::take(&mut unit.removed);
                    if came_back || *unit.machine.next_service() != service {
                        unit.machine.replace(service);
                        messages.push(format!("{name}: changed"));
                        changed.push(name);
                    }
                }
                None => {
                    messages.push(format!("{name}: new"));
                    self.units.push(Unit::new(service));
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
                if !self.units[slot].resting() {
                    messages.extend(self.restart(slot));
                }
            }
            let mut wanted = self.plan.pulled_in_by(DEFAULT_TARGET);
            if self.graphical {
                wanted.extend(self.plan.pulled_in_by(GRAPHICAL_TARGET));
            }
            for name in wanted {
                let Some(slot) = self.slot(&name) else {
                    continue;
                };
                let state = self.units[slot].machine.state();
                // A failed unit is tried again once its definition changes.
                let retry = state == State::Failed && changed.contains(&name);
                if state == State::Inactive || retry {
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
        }
    }

    // ---- adoption -------------------------------------------------------

    /// Take back the services a previous manager in this session left running.
    fn adopt(&mut self) {
        let Some(path) = self.state_path() else {
            return;
        };
        let saved = state::load(&path).unwrap_or_else(|e| {
            warning!("ignoring the saved state: {e}");
            Saved::default()
        });
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
            match pid {
                Some(pid) => info!("{name}: adopted, main process {pid}"),
                None => info!("{name}: adopted what is left of it; the main process is gone"),
            }
            let actions = self.units[slot]
                .machine
                .adopt(pid.is_some(), Instant::now());
            self.carry_out(slot, actions);
        }
        self.dirty = true;
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
        }
        self.dirty = true;
        self.save_if_dirty();
        let running = self.units.iter().filter(|u| u.job.is_some()).count();
        match self.exit {
            Some(Exit::Detach) => {
                info!("detached; {running} service(s) left running for the next manager")
            }
            _ => info!("stopped"),
        }
    }

    fn timeout(&self) -> Duration {
        let now = Instant::now();
        let mut timeout = if self.graphical { TICK } else { SHELL_POLL };
        for unit in &self.units {
            if let Some(deadline) = unit.machine.deadline() {
                timeout = timeout.min(deadline.saturating_duration_since(now));
            }
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
            DEFAULT_TARGET => true,
            GRAPHICAL_TARGET => self.graphical,
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
                    }
                }
            }
        }
    }

    fn check_shell(&mut self) {
        if !self.graphical && sys::shell_ready() {
            self.graphical = true;
            info!("the shell is ready: {GRAPHICAL_TARGET} reached");
            if self.exit.is_none() {
                let wanted = self.plan.pulled_in_by(GRAPHICAL_TARGET);
                self.want_started(wanted);
            }
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
            key if key >= KEY_JOB_BASE => {
                self.job_message(key - KEY_JOB_BASE, packet.bytes, packet.value as u32)
            }
            _ => {}
        }
    }

    fn process_exited(&mut self, token: usize) {
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
            JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS => {
                let code = process::exit_code_of(pid).unwrap_or(STATUS_UNSUCCESSFUL);
                self.mark(
                    slot,
                    &format!("process {pid} crashed (exception 0x{code:08X})"),
                );
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
        if self.units[slot].job.is_none() {
            self.new_job(slot)?;
        }
        let service = self.units[slot].machine.service().clone();
        let vars = env::merge(env::user_environment()?, &service.environment);
        let directory = service
            .working_directory
            .clone()
            .or_else(|| env::get(&vars, "USERPROFILE").map(str::to_owned))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let log = process::open_log(&self.log_path(slot).ok_or_else(no_state_dir)?)?;
        let stdin = self
            .stdin
            .as_ref()
            .ok_or_else(|| std::io::Error::other("no NUL handle"))?;
        let job = self.units[slot].job.as_ref().expect("created above");
        let child = process::spawn(
            &command.line,
            &env::block(&vars),
            &directory,
            job,
            stdin,
            &log,
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
        self.rotate_log(slot);
        let unit = &mut self.units[slot];
        unit.job = Some(job);
        unit.job_serial = serial;
        self.dirty = true;
        Ok(())
    }

    /// A unit at rest whose job is empty lets go of it.
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
    }

    fn report(&mut self, slot: usize, before: State) {
        let unit = &self.units[slot];
        let after = unit.machine.state();
        if after == before {
            return;
        }
        self.units[slot].since = (Instant::now(), crate::log::timestamp());
        let unit = &self.units[slot];
        let name = unit.name.clone();
        let last = unit.machine.last_outcome();
        let line = match after {
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
        } else {
            info!("{name}: {line}");
        }
        self.mark(slot, &line);
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

    fn rotate_log(&self, slot: usize) {
        let Some(path) = self.log_path(slot) else {
            return;
        };
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
    }

    /// A line from steward itself in the unit's log, between its output.
    fn mark(&self, slot: usize, message: &str) {
        let Some(path) = self.log_path(slot) else {
            return;
        };
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "-- {} steward: {message}", crate::log::timestamp());
        }
    }

    fn save_if_dirty(&mut self) {
        if !std::mem::take(&mut self.dirty) {
            return;
        }
        let Some(path) = self.state_path() else {
            return;
        };
        let mut saved = Saved::default();
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
            saved
                .units
                .insert(unit.name.clone(), SavedUnit { main, processes });
        }
        if let Err(e) = state::save(&path, &saved) {
            error!("cannot save the state to {}: {e}", path.display());
        }
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

fn no_state_dir() -> std::io::Error {
    std::io::Error::other("LOCALAPPDATA is not set; nowhere to put the unit's log")
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
/// every service and exits; a second Ctrl+C exits leaving them running.
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
    run(port, controls, inbox);
}
