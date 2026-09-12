//! One service's life: starting (`ExecStartPre=`, the main process,
//! `ExecStartPost=`), running, ending -- asked to or not -- and what follows:
//! a restart after a delay, `failed`, or rest.
//!
//! The machine is pure. It is fed [`Event`]s and answers with [`Action`]s;
//! starting processes, managing the job and keeping time are the caller's
//! business. The caller's side of the contract:
//!
//! - Every `Spawn*` action is answered, before anything else is fed, with
//!   `Spawned` or `SpawnFailed` for that process.
//! - Every process that was `Spawned` is reported `Exited` exactly once.
//! - `JobEmpty` is reported whenever the service's job has no processes left;
//!   repeating it is harmless, missing it is not. When an exit leaves the job
//!   empty, `JobEmpty` goes first, so that the machine does not ask processes
//!   to exit that are no longer there.
//! - `Deadline` is fed once [`Machine::deadline`] has passed.

use std::time::{Duration, Instant};

use steward_unit::{Command, Service, ServiceType};

use crate::policy::{self, classify, Outcome, StartLimit};

/// How long terminated processes get to disappear before steward stops
/// waiting for them.
pub const KILL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Inactive,
    Failed,
    /// Waiting out the delay before an automatic restart.
    AutoRestart,
    /// Running `ExecStartPre=` number n.
    StartPre(usize),
    /// Starting the main process; for `Type=forking`, until it exits.
    Starting,
    /// `Type=oneshot`: running `ExecStart=` number n.
    Oneshot(usize),
    /// The main process is up; running `ExecStartPost=` number n.
    StartPost(usize),
    Active,
    /// Running `ExecStop=` number n.
    StopExec(usize),
    /// The processes have been asked to exit.
    StopAsked,
    /// The processes have been terminated.
    StopKilled,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Inactive => "inactive",
            State::Failed => "failed",
            State::AutoRestart => "auto-restart",
            State::StartPre(_) => "start-pre",
            State::Starting | State::Oneshot(_) => "start",
            State::StartPost(_) => "start-post",
            State::Active => "active",
            State::StopExec(_) => "stop",
            State::StopAsked => "stop-asked",
            State::StopKilled => "stop-killed",
        }
    }

    /// On the way up: units ordered after this one wait for it.
    pub fn is_starting(self) -> bool {
        matches!(
            self,
            State::StartPre(_) | State::Starting | State::Oneshot(_) | State::StartPost(_)
        )
    }

    pub fn is_stopping(self) -> bool {
        matches!(
            self,
            State::StopExec(_) | State::StopAsked | State::StopKilled
        )
    }
}

/// Which of a service's processes an event is about. The main process is
/// `ExecStart=`; a control process is `ExecStartPre=`, `ExecStartPost=` or
/// `ExecStop=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Process {
    Main,
    Control,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Start,
    Stop,
    Spawned(Process),
    SpawnFailed(Process),
    Exited(Process, u32),
    /// The service's job has no processes left.
    JobEmpty,
    /// Some process in the job ended with an NTSTATUS error -- an exception,
    /// or a Ctrl+C -- (how a `Type=forking` daemon's crash is noticed, its
    /// exit code being nobody's to collect).
    Crashed(u32),
    Deadline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SpawnMain(Command),
    SpawnControl(Command),
    /// Ask every process in the job to exit: Ctrl+C to console programs,
    /// WM_CLOSE to windows.
    AskToExit,
    /// Terminate every process in the job.
    Kill,
    /// Terminate the control process (it outlived its deadline or its phase).
    KillControl,
}

#[derive(Debug, Clone, Copy)]
struct Ending {
    requested: bool,
    outcome: Outcome,
}

#[derive(Debug, Clone)]
pub struct Machine {
    service: Service,
    state: State,
    last: Option<Outcome>,
    restarts: u32,
    backoff_step: u32,
    start_limit: StartLimit,
    deadline: Option<Instant>,
    start_deadline: Option<Instant>,
    stop_deadline: Option<Instant>,
    active_since: Option<Instant>,
    main: bool,
    control: bool,
    job_empty: bool,
    crashed: Option<u32>,
    ending: Option<Ending>,
    start_after_stop: bool,
    /// A definition waiting for the next start.
    pending: Option<Service>,
}

impl Machine {
    pub fn new(service: Service) -> Self {
        Machine {
            service,
            state: State::Inactive,
            last: None,
            restarts: 0,
            backoff_step: 0,
            start_limit: StartLimit::default(),
            deadline: None,
            start_deadline: None,
            stop_deadline: None,
            active_since: None,
            main: false,
            control: false,
            job_empty: true,
            crashed: None,
            ending: None,
            start_after_stop: false,
            pending: None,
        }
    }

    pub fn service(&self) -> &Service {
        &self.service
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// How the service last ended, if it has.
    pub fn last_outcome(&self) -> Option<Outcome> {
        self.last
    }

    /// Automatic restarts so far.
    pub fn restarts(&self) -> u32 {
        self.restarts
    }

    /// When to feed `Deadline`.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// A new definition for the service (its unit file changed). A service at
    /// rest takes it at once; a running one keeps the definition it was
    /// started with -- its `ExecStop=` included -- until its next start.
    pub fn replace(&mut self, service: Service) {
        // A target has nothing running that could still be the old one.
        if service.is_target()
            || matches!(
                self.state,
                State::Inactive | State::Failed | State::AutoRestart
            )
        {
            self.service = service;
            self.pending = None;
        } else {
            self.pending = Some(service);
        }
    }

    /// The definition the service will start with next.
    pub fn next_service(&self) -> &Service {
        self.pending.as_ref().unwrap_or(&self.service)
    }

    /// Running with a definition that has since changed.
    pub fn is_changed(&self) -> bool {
        self.pending.is_some()
    }

    /// Do not start after all: a unit this one requires failed.
    pub fn refuse(&mut self, outcome: Outcome) {
        if matches!(self.state, State::Inactive | State::Failed) {
            self.last = Some(outcome);
            self.state = State::Failed;
        }
    }

    /// Take over a service that was already running when the manager started
    /// (the manager crashed or was upgraded and its job survived).
    /// A target the last manager had reached is simply active again.
    pub fn adopt(&mut self, main_alive: bool, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        if self.service.is_target() {
            self.state = State::Active;
            self.active_since = Some(now);
            return out;
        }
        self.main = main_alive;
        self.control = false;
        self.job_empty = false;
        self.state = State::Active;
        self.active_since = Some(now);
        if !main_alive && self.service.service_type != ServiceType::Forking {
            // Its main process is gone; what is left of the job is not the service.
            self.end(false, Outcome::Vanished, now, &mut out);
        }
        out
    }

    pub fn handle(&mut self, event: Event, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        self.step(event, now, &mut out);
        out
    }

    fn step(&mut self, event: Event, now: Instant, out: &mut Vec<Action>) {
        use Event::*;
        use Process::*;
        use State::*;

        // A target runs nothing: started, it is active; stopped, it is not.
        if self.service.is_target() {
            match event {
                Start => {
                    self.state = Active;
                    self.last = None;
                    self.active_since = Some(now);
                }
                Stop => {
                    self.state = Inactive;
                    self.active_since = None;
                }
                _ => {}
            }
            return;
        }

        match event {
            Spawned(process) => {
                self.set_alive(process, true);
                self.job_empty = false;
            }
            SpawnFailed(process) | Exited(process, _) => self.set_alive(process, false),
            JobEmpty => self.job_empty = true,
            Crashed(code) => self.crashed = Some(code),
            Deadline if !self.deadline.is_some_and(|d| now >= d) => return,
            _ => {}
        }

        match (self.state, event) {
            (_, Start) => self.request_start(now, out),
            (_, Stop) => self.request_stop(now, out),

            (StartPre(i), Exited(Control, code)) => self.after_pre(i, classify(code), now, out),
            (StartPre(i), SpawnFailed(Control)) => {
                self.after_pre(i, Outcome::SpawnFailed, now, out)
            }

            (Starting | Oneshot(_), SpawnFailed(Main)) => {
                self.end(false, Outcome::SpawnFailed, now, out)
            }
            (Starting, Spawned(Main)) if !self.is_forking() => self.post(0, now, out),
            (Starting, Exited(Main, code)) => match classify(code) {
                Outcome::Clean => self.post(0, now, out),
                outcome => self.end(false, outcome, now, out),
            },
            (Oneshot(i), Exited(Main, code)) => {
                let outcome = classify(code);
                if outcome.is_clean() || self.service.exec_start[i].ignore_failure {
                    self.oneshot(i + 1, now, out);
                } else {
                    self.end(false, outcome, now, out);
                }
            }

            (StartPost(i), Exited(Control, code)) => self.after_post(i, classify(code), now, out),
            (StartPost(i), SpawnFailed(Control)) => {
                self.after_post(i, Outcome::SpawnFailed, now, out)
            }
            (StartPost(_) | Active, Exited(Main, code)) => {
                self.end(false, classify(code), now, out)
            }
            (Active, JobEmpty) if self.is_forking() => {
                let outcome = self.crashed.map_or(Outcome::Vanished, classify);
                self.end(false, outcome, now, out);
            }

            (StopExec(i), Exited(Control, _) | SpawnFailed(Control)) => {
                self.next_stop(i + 1, now, out)
            }
            (StopExec(_) | StopAsked | StopKilled, Exited(..) | SpawnFailed(_) | JobEmpty) => {
                if self.all_gone() {
                    self.finish(now, out);
                }
            }

            (AutoRestart, Deadline) => self.begin_start(now, out),
            (StartPre(_) | Starting | Oneshot(_) | StartPost(_), Deadline) => {
                self.end(false, Outcome::Timeout, now, out)
            }
            (StopExec(_), Deadline) => self.kill(now, out),
            (StopAsked, Deadline) => self.kill(now, out),
            (StopKilled, Deadline) => {
                // Terminated and still there: nothing more steward can do.
                // Carry on as if they had gone rather than wedge the service.
                self.main = false;
                self.control = false;
                self.job_empty = true;
                if let Some(ending) = self.ending.as_mut() {
                    ending.outcome = Outcome::Timeout;
                }
                self.finish(now, out);
            }
            _ => {}
        }
    }

    fn set_alive(&mut self, process: Process, alive: bool) {
        match process {
            Process::Main => self.main = alive,
            Process::Control => self.control = alive,
        }
    }

    fn is_forking(&self) -> bool {
        self.service.service_type == ServiceType::Forking
    }

    fn all_gone(&self) -> bool {
        !self.main && !self.control && self.job_empty
    }

    fn request_start(&mut self, now: Instant, out: &mut Vec<Action>) {
        match self.state {
            State::Inactive | State::Failed => {
                // Asked for by someone, so past failures do not count against it.
                self.start_limit.reset();
                self.backoff_step = 0;
                self.begin_start(now, out);
            }
            State::AutoRestart => self.begin_start(now, out),
            s if s.is_stopping() => self.start_after_stop = true,
            _ => {}
        }
    }

    fn request_stop(&mut self, now: Instant, out: &mut Vec<Action>) {
        self.start_after_stop = false;
        match self.state {
            State::Inactive | State::Failed => {}
            State::AutoRestart => {
                self.state = State::Inactive;
                self.deadline = None;
            }
            State::Active => {
                self.ending = Some(Ending {
                    requested: true,
                    outcome: Outcome::Clean,
                });
                self.stop_deadline = now.checked_add(self.service.timeout_stop);
                self.next_stop(0, now, out);
            }
            s if s.is_starting() => self.end(true, Outcome::Clean, now, out),
            _ => {
                // Already stopping; if that was a failure, it is now also a
                // stop someone wants, and so no reason to restart.
                if let Some(ending) = self.ending.as_mut() {
                    ending.requested = true;
                }
            }
        }
    }

    fn begin_start(&mut self, now: Instant, out: &mut Vec<Action>) {
        self.deadline = None;
        if let Some(service) = self.pending.take() {
            self.service = service;
        }
        if !self.start_limit.allow(&self.service, now) {
            self.last = Some(Outcome::StartLimit);
            self.state = State::Failed;
            return;
        }
        self.ending = None;
        self.crashed = None;
        self.start_deadline = now.checked_add(self.service.timeout_start);
        self.pre(0, now, out);
    }

    fn pre(&mut self, i: usize, now: Instant, out: &mut Vec<Action>) {
        match self.service.exec_start_pre.get(i) {
            Some(command) => {
                self.state = State::StartPre(i);
                self.deadline = self.start_deadline;
                out.push(Action::SpawnControl(command.clone()));
            }
            None => self.main_start(now, out),
        }
    }

    fn after_pre(&mut self, i: usize, outcome: Outcome, now: Instant, out: &mut Vec<Action>) {
        if outcome.is_clean() || self.service.exec_start_pre[i].ignore_failure {
            self.pre(i + 1, now, out);
        } else {
            self.end(false, outcome, now, out);
        }
    }

    fn main_start(&mut self, now: Instant, out: &mut Vec<Action>) {
        if self.service.service_type == ServiceType::Oneshot {
            return self.oneshot(0, now, out);
        }
        self.state = State::Starting;
        self.deadline = self.start_deadline;
        out.push(Action::SpawnMain(self.service.exec_start[0].clone()));
    }

    fn oneshot(&mut self, i: usize, now: Instant, out: &mut Vec<Action>) {
        match self.service.exec_start.get(i) {
            Some(command) => {
                self.state = State::Oneshot(i);
                self.deadline = self.start_deadline;
                out.push(Action::SpawnMain(command.clone()));
            }
            None => self.post(0, now, out),
        }
    }

    fn post(&mut self, i: usize, now: Instant, out: &mut Vec<Action>) {
        match self.service.exec_start_post.get(i) {
            Some(command) => {
                self.state = State::StartPost(i);
                self.deadline = self.start_deadline;
                out.push(Action::SpawnControl(command.clone()));
            }
            None => self.started(now, out),
        }
    }

    fn after_post(&mut self, i: usize, outcome: Outcome, now: Instant, out: &mut Vec<Action>) {
        if outcome.is_clean() || self.service.exec_start_post[i].ignore_failure {
            self.post(i + 1, now, out);
        } else {
            self.end(false, outcome, now, out);
        }
    }

    fn started(&mut self, now: Instant, out: &mut Vec<Action>) {
        match self.service.service_type {
            // A oneshot that has run all of its commands is done.
            ServiceType::Oneshot => self.end(false, Outcome::Clean, now, out),
            ServiceType::Forking if self.job_empty => {
                let outcome = self.crashed.map_or(Outcome::Vanished, classify);
                self.end(false, outcome, now, out);
            }
            _ => {
                self.state = State::Active;
                self.deadline = None;
                self.active_since = Some(now);
            }
        }
    }

    /// The service is ending, asked to or not: stop what is still running.
    fn end(&mut self, requested: bool, outcome: Outcome, now: Instant, out: &mut Vec<Action>) {
        self.ending = Some(Ending { requested, outcome });
        self.stop_deadline = now.checked_add(self.service.timeout_stop);
        if self.control {
            out.push(Action::KillControl);
        }
        self.ask(now, out);
    }

    fn next_stop(&mut self, i: usize, now: Instant, out: &mut Vec<Action>) {
        let still_running = self.main || !self.job_empty;
        match self.service.exec_stop.get(i) {
            Some(command) if still_running => {
                self.state = State::StopExec(i);
                self.deadline = self.stop_deadline;
                out.push(Action::SpawnControl(command.clone()));
            }
            _ => self.ask(now, out),
        }
    }

    fn ask(&mut self, now: Instant, out: &mut Vec<Action>) {
        if self.all_gone() {
            return self.finish(now, out);
        }
        self.state = State::StopAsked;
        self.deadline = self.stop_deadline;
        out.push(Action::AskToExit);
    }

    fn kill(&mut self, now: Instant, out: &mut Vec<Action>) {
        self.state = State::StopKilled;
        self.deadline = now.checked_add(KILL_TIMEOUT);
        out.push(Action::Kill);
    }

    fn finish(&mut self, now: Instant, out: &mut Vec<Action>) {
        let ending = self.ending.take().unwrap_or(Ending {
            requested: true,
            outcome: Outcome::Clean,
        });
        self.deadline = None;
        // A service that stayed up longer than the longest delay was not in a
        // crash loop: its next restart starts the backoff over.
        if let Some(since) = self.active_since.take() {
            let longest = self.service.restart_max_delay.max(self.service.restart_sec);
            if now.duration_since(since) >= longest {
                self.backoff_step = 0;
            }
        }
        self.last = Some(ending.outcome);

        if ending.requested {
            self.state = State::Inactive;
            if std::mem::take(&mut self.start_after_stop) {
                self.request_start(now, out);
            }
        } else if policy::should_restart(self.service.restart, ending.outcome) {
            self.state = State::AutoRestart;
            self.deadline =
                now.checked_add(policy::restart_delay(&self.service, self.backoff_step));
            self.backoff_step += 1;
            self.restarts += 1;
        } else if ending.outcome.is_clean() {
            self.state = State::Inactive;
        } else {
            self.state = State::Failed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Action::*;
    use super::Event::*;
    use super::Process::*;
    use super::*;
    use steward_unit::parse_service;

    const CRASH: u32 = 0xC000_0005;

    struct Harness {
        machine: Machine,
        now: Instant,
    }

    impl Harness {
        fn new(service_section: &str) -> Self {
            let text = format!("[Service]\n{service_section}");
            let parsed = parse_service("t.service", &text);
            assert!(!parsed.has_errors(), "{:?}", parsed.diagnostics);
            Harness {
                machine: Machine::new(parsed.service.unwrap()),
                now: Instant::now(),
            }
        }

        /// Feed an event and, like the real caller, answer every spawn
        /// action with Spawned straight away. Returns every action.
        fn feed(&mut self, event: Event) -> Vec<Action> {
            let mut actions = self.machine.handle(event, self.now);
            let mut i = 0;
            while i < actions.len() {
                let reply = match &actions[i] {
                    SpawnMain(_) => Some(Spawned(Main)),
                    SpawnControl(_) => Some(Spawned(Control)),
                    _ => None,
                };
                if let Some(reply) = reply {
                    actions.extend(self.machine.handle(reply, self.now));
                }
                i += 1;
            }
            actions
        }

        /// Feed without answering spawns (to answer them some other way).
        fn raw(&mut self, event: Event) -> Vec<Action> {
            self.machine.handle(event, self.now)
        }

        fn wait(&mut self, secs: f64) -> Vec<Action> {
            self.now += Duration::from_secs_f64(secs);
            match self.machine.deadline() {
                Some(d) if self.now >= d => self.feed(Deadline),
                _ => Vec::new(),
            }
        }

        fn state(&self) -> State {
            self.machine.state()
        }

        fn delay(&self) -> f64 {
            let d = self
                .machine
                .deadline()
                .expect("a deadline")
                .duration_since(self.now);
            (d.as_secs_f64() * 10.0).round() / 10.0
        }

        /// Move time to the deadline exactly and feed it.
        fn until_deadline(&mut self) -> Vec<Action> {
            self.now = self.machine.deadline().expect("a deadline");
            self.feed(Deadline)
        }

        /// The main process exits with `code`, leaving the job empty.
        fn main_exits(&mut self, code: u32) -> Vec<Action> {
            let mut actions = self.feed(JobEmpty);
            actions.extend(self.feed(Exited(Main, code)));
            actions
        }
    }

    fn cmd(line: &str) -> Command {
        Command {
            line: line.into(),
            ignore_failure: false,
        }
    }

    #[test]
    fn a_simple_service_starts_and_is_active() {
        let mut h = Harness::new("ExecStart=app.exe\n");
        assert_eq!(h.feed(Start), [SpawnMain(cmd("app.exe"))]);
        assert_eq!(h.state(), State::Active);
        assert_eq!(h.machine.deadline(), None);
    }

    #[test]
    fn a_crash_is_restarted_after_a_second() {
        let mut h = Harness::new("ExecStart=app.exe\n");
        h.feed(Start);
        assert_eq!(h.main_exits(CRASH), []);
        assert_eq!(h.state(), State::AutoRestart);
        assert_eq!(h.machine.last_outcome(), Some(Outcome::Crashed(CRASH)));
        assert_eq!(h.delay(), 1.0);
        assert_eq!(h.wait(0.5), []);
        assert_eq!(h.wait(0.5), [SpawnMain(cmd("app.exe"))]);
        assert_eq!(h.state(), State::Active);
        assert_eq!(h.machine.restarts(), 1);
    }

    #[test]
    fn an_exit_reported_before_the_empty_job_still_ends_the_same_way() {
        let mut h = Harness::new("ExecStart=app.exe\n");
        h.feed(Start);
        // Asking an empty job to exit is harmless.
        assert_eq!(h.feed(Exited(Main, 1)), [AskToExit]);
        assert_eq!(h.feed(JobEmpty), []);
        assert_eq!(h.state(), State::AutoRestart);
        assert_eq!(h.machine.last_outcome(), Some(Outcome::ExitCode(1)));
    }

    #[test]
    fn leftover_processes_are_asked_to_exit_then_killed() {
        let mut h = Harness::new("ExecStart=app.exe\nTimeoutStopSec=3s\n");
        h.feed(Start);
        // The main process died, but a child it started is still in the job.
        assert_eq!(h.feed(Exited(Main, CRASH)), [AskToExit]);
        assert_eq!(h.state(), State::StopAsked);
        assert_eq!(h.wait(3.0), [Kill]);
        assert_eq!(h.state(), State::StopKilled);
        h.feed(JobEmpty);
        assert_eq!(h.state(), State::AutoRestart);
        assert_eq!(h.machine.last_outcome(), Some(Outcome::Crashed(CRASH)));
    }

    const CTRL_C: u32 = 0xC000_013A;

    #[test]
    fn a_ctrl_c_from_elsewhere_is_restarted() {
        let mut h = Harness::new("ExecStart=app.exe\n");
        h.feed(Start);
        assert_eq!(h.main_exits(CTRL_C), []);
        assert_eq!(h.state(), State::AutoRestart);
        assert_eq!(h.machine.last_outcome(), Some(Outcome::Interrupted));
    }

    #[test]
    fn a_forking_daemon_ended_by_ctrl_c_was_interrupted() {
        let mut h = Harness::new("Type=forking\nExecStart=launcher.exe\n");
        h.feed(Start);
        h.feed(Exited(Main, 0));
        assert_eq!(h.state(), State::Active);
        h.feed(Crashed(CTRL_C));
        h.feed(JobEmpty);
        assert_eq!(h.state(), State::AutoRestart);
        assert_eq!(h.machine.last_outcome(), Some(Outcome::Interrupted));
    }

    #[test]
    fn the_ctrl_c_of_a_stop_is_a_clean_end() {
        let mut h = Harness::new("ExecStart=app.exe\n");
        h.feed(Start);
        assert_eq!(h.feed(Stop), [AskToExit]);
        h.main_exits(CTRL_C);
        assert_eq!(h.state(), State::Inactive);
        assert_eq!(h.machine.last_outcome(), Some(Outcome::Clean));
    }

    #[test]
    fn a_clean_exit_is_not_restarted_by_default() {
        let mut h = Harness::new("ExecStart=app.exe\n");
        h.feed(Start);
        h.main_exits(0);
        assert_eq!(h.state(), State::Inactive);
        assert_eq!(h.machine.deadline(), None);
    }

    #[test]
    fn restart_no_leaves_a_failure_failed() {
        let mut h = Harness::new("ExecStart=app.exe\nRestart=no\n");
        h.feed(Start);
        h.main_exits(2);
        assert_eq!(h.state(), State::Failed);
    }

    #[test]
    fn the_backoff_grows_while_it_keeps_crashing_and_resets_after_a_good_run() {
        let mut h = Harness::new("ExecStart=app.exe\n");
        h.feed(Start);
        let mut delays = Vec::new();
        for _ in 0..7 {
            h.main_exits(CRASH);
            delays.push(h.delay());
            h.until_deadline();
        }
        assert_eq!(delays, [1.0, 2.3, 5.1, 11.7, 26.5, 60.0, 60.0]);
        // Up for a minute: the next crash is back to a second.
        h.wait(60.0);
        h.main_exits(CRASH);
        assert_eq!(h.delay(), 1.0);
    }

    #[test]
    fn the_start_limit_fails_a_fast_crash_loop() {
        let mut h =
            Harness::new("ExecStart=app.exe\nRestartSec=0\nRestartSteps=0\nStartLimitBurst=3\n");
        h.feed(Start);
        for _ in 0..2 {
            h.main_exits(CRASH);
            h.wait(0.0);
            assert_eq!(h.state(), State::Active);
        }
        h.main_exits(CRASH);
        assert_eq!(h.wait(0.0), []);
        assert_eq!(h.state(), State::Failed);
        assert_eq!(h.machine.last_outcome(), Some(Outcome::StartLimit));
        // Someone starting it by hand is not held to the limit.
        assert_eq!(h.feed(Start), [SpawnMain(cmd("app.exe"))]);
    }

    #[test]
    fn a_requested_stop_runs_exec_stop_then_asks() {
        let mut h = Harness::new("ExecStart=app.exe\nExecStop=app.exe --quit\n");
        h.feed(Start);
        assert_eq!(h.feed(Stop), [SpawnControl(cmd("app.exe --quit"))]);
        assert_eq!(h.state(), State::StopExec(0));
        // ExecStop finished but the main process is still there.
        assert_eq!(h.feed(Exited(Control, 0)), [AskToExit]);
        assert_eq!(h.main_exits(0), []);
        assert_eq!(h.state(), State::Inactive);
    }

    #[test]
    fn exec_stop_that_works_needs_no_asking() {
        let mut h = Harness::new("ExecStart=app.exe\nExecStop=app.exe --quit\n");
        h.feed(Start);
        h.feed(Stop);
        assert_eq!(h.feed(Exited(Main, 0)), []);
        // ExecStop's exit empties the job.
        assert_eq!(h.feed(JobEmpty), []);
        assert_eq!(h.feed(Exited(Control, 0)), []);
        assert_eq!(h.state(), State::Inactive);
    }

    #[test]
    fn a_stop_that_is_ignored_is_a_kill_and_still_not_a_failure() {
        let mut h = Harness::new("ExecStart=app.exe\nTimeoutStopSec=2s\n");
        h.feed(Start);
        assert_eq!(h.feed(Stop), [AskToExit]);
        assert_eq!(h.wait(2.0), [Kill]);
        h.main_exits(1);
        assert_eq!(h.state(), State::Inactive);
        assert_eq!(h.machine.deadline(), None);
    }

    #[test]
    fn processes_that_survive_a_kill_are_given_up_on() {
        let mut h = Harness::new("ExecStart=app.exe\nTimeoutStopSec=1s\n");
        h.feed(Start);
        h.feed(Stop);
        h.wait(1.0);
        assert_eq!(h.state(), State::StopKilled);
        h.wait(KILL_TIMEOUT.as_secs_f64());
        assert_eq!(h.state(), State::Inactive);
        assert_eq!(h.machine.last_outcome(), Some(Outcome::Timeout));
    }

    #[test]
    fn exec_start_pre_runs_first_and_its_failure_is_a_failure() {
        let mut h = Harness::new("ExecStartPre=prep.exe\nExecStart=app.exe\n");
        assert_eq!(h.feed(Start), [SpawnControl(cmd("prep.exe"))]);
        assert_eq!(h.state(), State::StartPre(0));
        h.feed(Exited(Control, 3));
        h.feed(JobEmpty);
        assert_eq!(h.state(), State::AutoRestart);
        assert_eq!(h.machine.last_outcome(), Some(Outcome::ExitCode(3)));
    }

    #[test]
    fn a_dash_lets_a_command_fail() {
        let mut h =
            Harness::new("ExecStartPre=-prep.exe\nExecStart=app.exe\nExecStartPost=-post.exe\n");
        h.feed(Start);
        assert_eq!(
            h.feed(Exited(Control, 1)),
            [
                SpawnMain(cmd("app.exe")),
                SpawnControl(Command {
                    line: "post.exe".into(),
                    ignore_failure: true
                })
            ]
        );
        assert_eq!(h.state(), State::StartPost(0));
        h.feed(Exited(Control, 1));
        assert_eq!(h.state(), State::Active);
    }

    #[test]
    fn a_start_that_hangs_times_out() {
        let mut h = Harness::new("ExecStartPre=prep.exe\nExecStart=app.exe\nTimeoutStartSec=5s\n");
        h.feed(Start);
        assert_eq!(h.wait(5.0), [KillControl, AskToExit]);
        h.feed(Exited(Control, 1));
        h.feed(JobEmpty);
        assert_eq!(h.state(), State::AutoRestart);
        assert_eq!(h.machine.last_outcome(), Some(Outcome::Timeout));
    }

    #[test]
    fn a_missing_program_is_retried() {
        let mut h = Harness::new("ExecStart=missing.exe\n");
        assert_eq!(h.raw(Start), [SpawnMain(cmd("missing.exe"))]);
        assert_eq!(h.raw(SpawnFailed(Main)), []);
        assert_eq!(h.state(), State::AutoRestart);
        assert_eq!(h.machine.last_outcome(), Some(Outcome::SpawnFailed));
    }

    #[test]
    fn a_forking_service_lives_in_its_job() {
        let mut h = Harness::new("Type=forking\nExecStart=launcher.exe start\n");
        h.feed(Start);
        assert_eq!(h.state(), State::Starting);
        h.feed(Exited(Main, 0));
        assert_eq!(h.state(), State::Active);
        // The daemon it launched crashes.
        h.feed(Crashed(CRASH));
        h.feed(JobEmpty);
        assert_eq!(h.state(), State::AutoRestart);
        assert_eq!(h.machine.last_outcome(), Some(Outcome::Crashed(CRASH)));
        // Or simply goes away.
        h.until_deadline();
        h.feed(Exited(Main, 0));
        h.feed(JobEmpty);
        assert_eq!(h.machine.last_outcome(), Some(Outcome::Vanished));
    }

    #[test]
    fn a_forking_launcher_that_fails_fails_the_start() {
        let mut h = Harness::new("Type=forking\nExecStart=launcher.exe start\n");
        h.feed(Start);
        h.main_exits(1);
        assert_eq!(h.machine.last_outcome(), Some(Outcome::ExitCode(1)));
    }

    #[test]
    fn a_oneshot_runs_its_commands_in_order_and_rests() {
        let mut h = Harness::new("Type=oneshot\nExecStart=one.exe\nExecStart=two.exe\n");
        assert_eq!(h.feed(Start), [SpawnMain(cmd("one.exe"))]);
        assert_eq!(h.feed(Exited(Main, 0)), [SpawnMain(cmd("two.exe"))]);
        h.main_exits(0);
        assert_eq!(h.state(), State::Inactive);
        assert_eq!(h.machine.last_outcome(), Some(Outcome::Clean));
    }

    #[test]
    fn stopping_during_the_restart_delay_cancels_it() {
        let mut h = Harness::new("ExecStart=app.exe\n");
        h.feed(Start);
        h.main_exits(CRASH);
        h.feed(Stop);
        assert_eq!(h.state(), State::Inactive);
        assert_eq!(h.wait(120.0), []);
    }

    #[test]
    fn a_stop_during_failure_cleanup_prevents_the_restart() {
        let mut h = Harness::new("ExecStart=app.exe\n");
        h.feed(Start);
        h.feed(Exited(Main, CRASH));
        assert_eq!(h.state(), State::StopAsked);
        h.feed(Stop);
        h.feed(JobEmpty);
        assert_eq!(h.state(), State::Inactive);
    }

    #[test]
    fn a_start_while_stopping_starts_again_afterwards() {
        let mut h = Harness::new("ExecStart=app.exe\n");
        h.feed(Start);
        h.feed(Stop);
        assert_eq!(h.feed(Start), []);
        assert_eq!(h.main_exits(0), [SpawnMain(cmd("app.exe"))]);
        assert_eq!(h.state(), State::Active);
    }

    #[test]
    fn stopping_while_starting_skips_exec_stop() {
        let mut h = Harness::new("ExecStartPre=prep.exe\nExecStart=app.exe\nExecStop=quit.exe\n");
        h.feed(Start);
        assert_eq!(h.feed(Stop), [KillControl, AskToExit]);
        h.feed(Exited(Control, 1));
        h.feed(JobEmpty);
        assert_eq!(h.state(), State::Inactive);
    }

    fn target(text: &str) -> Machine {
        Machine::new(parse_service("tiling.target", text).service.unwrap())
    }

    #[test]
    fn a_target_is_active_once_started_and_runs_nothing() {
        let mut t = target("[Unit]\nWants=a.service\n");
        let now = Instant::now();
        assert_eq!(t.handle(Start, now), []);
        assert_eq!(t.state(), State::Active);
        assert_eq!(t.deadline(), None);
        assert_eq!(t.handle(Stop, now), []);
        assert_eq!(t.state(), State::Inactive);
        assert_eq!(t.adopt(false, now), []);
        assert_eq!(t.state(), State::Active);
    }

    #[test]
    fn a_target_takes_a_new_definition_at_once() {
        let mut t = target("[Unit]\n");
        t.handle(Start, Instant::now());
        let changed = parse_service("tiling.target", "[Unit]\nDescription=new\n")
            .service
            .unwrap();
        t.replace(changed);
        assert!(!t.is_changed());
        assert_eq!(t.service().description.as_deref(), Some("new"));
        assert_eq!(t.state(), State::Active);
    }

    #[test]
    fn adopting_a_running_service() {
        let mut h = Harness::new("ExecStart=app.exe\n");
        assert_eq!(h.machine.adopt(true, h.now), []);
        assert_eq!(h.state(), State::Active);
        h.main_exits(CRASH);
        assert_eq!(h.state(), State::AutoRestart);
    }

    #[test]
    fn adopting_a_job_whose_main_process_is_gone() {
        let mut h = Harness::new("ExecStart=app.exe\n");
        assert_eq!(h.machine.adopt(false, h.now), [AskToExit]);
        h.feed(JobEmpty);
        assert_eq!(h.state(), State::AutoRestart);
        assert_eq!(h.machine.last_outcome(), Some(Outcome::Vanished));
    }

    #[test]
    fn a_new_definition_waits_for_the_next_start() {
        let mut h = Harness::new("ExecStart=old.exe\nExecStop=old.exe --quit\n");
        h.feed(Start);
        let new = parse_service("t.service", "[Service]\nExecStart=new.exe\n")
            .service
            .unwrap();
        h.machine.replace(new);
        assert_eq!(h.machine.next_service().exec_start[0].line, "new.exe");
        // The running service stops the way it was started.
        assert_eq!(h.feed(Stop), [SpawnControl(cmd("old.exe --quit"))]);
        h.feed(Start);
        h.feed(JobEmpty);
        h.feed(Exited(Main, 0));
        assert_eq!(h.feed(Exited(Control, 0)), [SpawnMain(cmd("new.exe"))]);
    }

    #[test]
    fn a_resting_service_takes_a_new_definition_at_once() {
        let mut h = Harness::new("ExecStart=old.exe\n");
        let new = parse_service("t.service", "[Service]\nExecStart=new.exe\n")
            .service
            .unwrap();
        h.machine.replace(new);
        assert_eq!(h.machine.service().exec_start[0].line, "new.exe");
    }

    #[test]
    fn stale_deadlines_are_ignored() {
        let mut h = Harness::new("ExecStart=app.exe\n");
        h.feed(Start);
        assert_eq!(h.raw(Deadline), []);
        assert_eq!(h.state(), State::Active);
    }
}
