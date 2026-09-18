//! A unit file: the keys steward understands, their defaults, and a
//! diagnostic for everything else.
//!
//! A `.service` runs something. A `.target` runs nothing: it is `[Unit]` and
//! `[Install]` only, a name units can be `WantedBy=`, ordered `After=`, and
//! `PartOf=`, so that starting or stopping it starts or stops them. A
//! `.timer` runs nothing itself either: its `[Timer]` section says when to
//! start another unit. All three are a [`Service`], told apart by its
//! [`UnitKind`]. `default.target`, `graphical-session.target`, `tray.target`
//! and `timers.target` are steward's own.
//!
//! Assignment follows systemd: a scalar key takes its last value; a list key
//! (`After=`, `Environment=`, `ExecStartPre=`, ...) accumulates, and an empty
//! assignment resets it. Keys and sections steward does not know are warnings,
//! so a unit written for Linux still loads and says what it ignored; `X-`
//! sections and keys are ignored silently, as systemd does. Every key, known
//! or not, is kept as written in [`Service::entries`], for `switch` to
//! compare.
//!
//! Where Windows differs from systemd:
//! - `Exec*=` values are Windows command lines, passed to `CreateProcessW` as
//!   written. A leading `-` (ignore failure) is the only prefix.
//! - `Environment=` groups words with `"` or `'`; a backslash is an ordinary
//!   character.
//!
//! The defaults favour durability, which is the point of steward, over
//! systemd's:
//! - `Restart=on-failure` (systemd: `no`). A service that crashes comes back
//!   unless its unit says otherwise.
//! - Backoff: `RestartSec=1s`, `RestartSteps=5`, `RestartMaxDelaySec=1min`
//!   (systemd: 100 ms, no backoff). Delays grow 1 s, 2.3 s, 5.1 s ... to a
//!   minute; with the default start limit (5 starts in 10 s) that means a
//!   service that keeps failing keeps being retried, a minute apart, rather
//!   than giving up after half a second.
//! - `TimeoutStartSec=30s`, `TimeoutStopSec=10s` (systemd: 90 s each): a stop
//!   at sign-out does not get to wait a minute and a half.

use std::fmt;
use std::time::Duration;

use crate::calendar::parse_calendar;
use crate::compare::{Entries, SwitchMethod};
use crate::syntax::{self, Entry, UnitFile};
use crate::time::parse_timespan;
use crate::timer::{Timer, Trigger};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// 1-based; 0 means the file as a whole.
    pub line: usize,
    pub severity: Severity,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let severity = match self.severity {
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        if self.line == 0 {
            write!(f, "{severity}: {}", self.message)
        } else {
            write!(f, "line {}: {severity}: {}", self.line, self.message)
        }
    }
}

/// Reached as soon as the manager is up, at sign-in.
pub const DEFAULT_TARGET: &str = "default.target";
/// Reached once the shell is ready: Explorer's taskbar exists.
pub const GRAPHICAL_TARGET: &str = "graphical-session.target";
/// home-manager's name for "the tray is there": reached once the tray takes
/// icons, when Explorer broadcasts `TaskbarCreated` -- about a second after
/// `graphical-session.target`, at sign-in.
pub const TRAY_TARGET: &str = "tray.target";
/// Where timers are installed. systemd reaches it early in a user manager's
/// start; here it is another name for `default.target`.
pub const TIMERS_TARGET: &str = "timers.target";
/// The targets steward reaches itself; no unit file may be one of them.
pub const BUILTIN_TARGETS: [&str; 4] =
    [DEFAULT_TARGET, GRAPHICAL_TARGET, TRAY_TARGET, TIMERS_TARGET];

/// A service; a target, a unit that runs nothing; or a timer, which starts
/// another unit when it elapses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnitKind {
    #[default]
    Service,
    Target,
    Timer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServiceType {
    /// The service is up as soon as its process is created.
    #[default]
    Simple,
    /// The service is up once `CreateProcessW` has succeeded.
    Exec,
    /// The main process starts the real one and exits; the service lives while
    /// its job has processes.
    Forking,
    /// Runs to completion; `ExecStart=` may repeat.
    Oneshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Restart {
    No,
    OnSuccess,
    #[default]
    OnFailure,
    OnAbnormal,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KillMode {
    /// The job tracks every descendant, and a stop ends them all.
    #[default]
    ControlGroup,
    /// Only the main process is the service's; its children break away from the
    /// job and belong to the user (a hotkey daemon's terminals, a launcher's
    /// applications).
    Process,
}

/// Where a service's standard output and error go: `StandardOutput=`.
///
/// A unit that does not say has no `Output` ([`Service::standard_output`] is
/// `None`). Its output goes to the Event Log channel on a machine that has
/// one and to its file on a machine that does not, and only the program
/// running it can tell which machine it is on, so the parser leaves the
/// choice open rather than guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// `%LOCALAPPDATA%\steward\logs\<unit>.log`, through an inherited
    /// handle: always there, whatever the machine has.
    File,
    /// The user's Event Log channel, `Steward/<SID>`, through a
    /// `steward-cat` shim the manager starts for the unit.
    EventLog,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// A Windows command line, as written.
    pub line: String,
    /// `-` prefix: a failure of this command does not fail the service.
    pub ignore_failure: bool,
}

/// A unit: a service; a target, which has the `[Unit]` and `[Install]`
/// fields and nothing to run; or a timer, which has those and a [`Timer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// The unit's name, `whkd.service`, `tiling.target` or `backup.timer`.
    pub name: String,
    pub kind: UnitKind,
    pub description: Option<String>,
    pub documentation: Vec<String>,
    pub after: Vec<String>,
    pub before: Vec<String>,
    pub wants: Vec<String>,
    pub requires: Vec<String>,
    /// Stopping or restarting any of these stops or restarts this unit too.
    pub part_of: Vec<String>,
    pub start_limit_burst: u32,
    pub start_limit_interval: Duration,

    pub service_type: ServiceType,
    pub exec_start: Vec<Command>,
    pub exec_start_pre: Vec<Command>,
    pub exec_start_post: Vec<Command>,
    pub exec_stop: Vec<Command>,
    /// Run one after another while the main process keeps running, to have
    /// it take its configuration again.
    pub exec_reload: Vec<Command>,
    pub restart: Restart,
    /// The delay before the first automatic restart.
    pub restart_sec: Duration,
    /// Restarts it takes for the delay to grow from `restart_sec` to
    /// `restart_max_delay`; 0 turns the backoff off.
    pub restart_steps: u32,
    pub restart_max_delay: Duration,
    pub timeout_start: Duration,
    pub timeout_stop: Duration,
    /// Unset means the user's profile directory.
    pub working_directory: Option<String>,
    pub environment: Vec<(String, String)>,
    pub kill_mode: KillMode,
    /// Where its output goes, if the unit says; `None` if it does not (see
    /// [`Output`]). Its error stream goes with it: there is no
    /// `StandardError=`.
    pub standard_output: Option<Output>,

    /// A timer's `[Timer]` section; `None` for anything else.
    pub timer: Option<Timer>,

    pub wanted_by: Vec<String>,

    /// The file as written, the keys the parser skips included: what
    /// `switch` compares to tell a restart from a reload ([`crate::compare`]).
    pub entries: Entries,
}

impl Service {
    pub fn is_target(&self) -> bool {
        self.kind == UnitKind::Target
    }

    pub fn is_timer(&self) -> bool {
        self.kind == UnitKind::Timer
    }

    /// A target or a timer: started, it is active; stopped, it is not; and
    /// there is never a process of its own.
    pub fn runs_nothing(&self) -> bool {
        self.kind != UnitKind::Service
    }

    fn new(name: &str, kind: UnitKind) -> Self {
        Service {
            name: name.to_owned(),
            kind,
            description: None,
            documentation: Vec::new(),
            after: Vec::new(),
            before: Vec::new(),
            wants: Vec::new(),
            requires: Vec::new(),
            part_of: Vec::new(),
            start_limit_burst: 5,
            start_limit_interval: Duration::from_secs(10),
            service_type: ServiceType::Simple,
            exec_start: Vec::new(),
            exec_start_pre: Vec::new(),
            exec_start_post: Vec::new(),
            exec_stop: Vec::new(),
            exec_reload: Vec::new(),
            restart: Restart::OnFailure,
            restart_sec: Duration::from_secs(1),
            restart_steps: 5,
            restart_max_delay: Duration::from_secs(60),
            timeout_start: Duration::from_secs(30),
            timeout_stop: Duration::from_secs(10),
            working_directory: None,
            environment: Vec::new(),
            kill_mode: KillMode::ControlGroup,
            standard_output: None,
            timer: (kind == UnitKind::Timer).then(|| Timer::new(name)),
            wanted_by: Vec::new(),
            entries: Entries::default(),
        }
    }
}

/// The result of reading a unit: the unit, unless a diagnostic is an error.
#[derive(Debug, Clone)]
pub struct Parsed {
    pub service: Option<Service>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Parsed {
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }
}

/// Read `text` as the unit called `name` -- its file name, `whkd.service`,
/// `tiling.target` or `backup.timer`, which says which kind of unit it is.
pub fn parse_service(name: &str, text: &str) -> Parsed {
    let mut reader = Reader {
        diagnostics: Vec::new(),
        bad_exec_start: false,
        bad_trigger: false,
    };
    let service = match syntax::parse(text) {
        Ok(file) => reader.service(name, &file),
        Err(e) => {
            reader.error(e.line, e.message);
            None
        }
    };
    let failed = reader
        .diagnostics
        .iter()
        .any(|d| d.severity == Severity::Error);
    Parsed {
        service: if failed { None } else { service },
        diagnostics: reader.diagnostics,
    }
}

struct Reader {
    diagnostics: Vec<Diagnostic>,
    /// An ExecStart= line was already an error: don't also report the count.
    bad_exec_start: bool,
    /// So was a timer's trigger: don't also report that it has none.
    bad_trigger: bool,
}

impl Reader {
    fn warn(&mut self, line: usize, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            line,
            severity: Severity::Warning,
            message: message.into(),
        });
    }

    fn error(&mut self, line: usize, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            line,
            severity: Severity::Error,
            message: message.into(),
        });
    }

    fn service(&mut self, name: &str, file: &UnitFile) -> Option<Service> {
        let kind = if name.ends_with(".target") {
            UnitKind::Target
        } else if name.ends_with(".timer") {
            UnitKind::Timer
        } else {
            if !name.ends_with(".service") {
                self.error(
                    0,
                    format!("{name:?} is not a .service, .target or .timer unit"),
                );
            }
            UnitKind::Service
        };
        if BUILTIN_TARGETS.contains(&name) {
            self.error(
                0,
                format!("{name} is steward's own target; name yours otherwise"),
            );
        }
        let mut s = Service::new(name, kind);
        s.entries = Entries::of(file);
        for section in &file.sections {
            match section.name.as_str() {
                "Unit" => section
                    .entries
                    .iter()
                    .for_each(|e| self.unit_key(&mut s, e)),
                "Service" if kind == UnitKind::Target => self.error(
                    section.line,
                    "a target runs nothing; it has no [Service] section",
                ),
                "Service" if kind == UnitKind::Timer => self.error(
                    section.line,
                    "a timer runs nothing itself; it has no [Service] section (Unit= names what it starts)",
                ),
                "Service" => section
                    .entries
                    .iter()
                    .for_each(|e| self.service_key(&mut s, e)),
                "Timer" if kind == UnitKind::Timer => {
                    let mut timer = s.timer.take().expect("a timer has a [Timer]");
                    section
                        .entries
                        .iter()
                        .for_each(|e| self.timer_key(&mut timer, e));
                    s.timer = Some(timer);
                }
                "Install" => section
                    .entries
                    .iter()
                    .for_each(|e| self.install_key(&mut s, e)),
                other if other.starts_with("X-") => {}
                other => self.warn(
                    section.line,
                    format!("section [{other}] is not supported; ignored"),
                ),
            }
        }
        self.validate(&s);
        Some(s)
    }

    fn unit_key(&mut self, s: &mut Service, e: &Entry) {
        match e.key.as_str() {
            "Description" => s.description = non_empty(&e.value),
            "Documentation" => list(&mut s.documentation, &e.value),
            "After" => list(&mut s.after, &e.value),
            "Before" => list(&mut s.before, &e.value),
            "Wants" => list(&mut s.wants, &e.value),
            "Requires" => list(&mut s.requires, &e.value),
            "PartOf" => list(&mut s.part_of, &e.value),
            "StartLimitBurst" => self.number(e, &mut s.start_limit_burst),
            "StartLimitIntervalSec" => self.span(e, &mut s.start_limit_interval),
            // Read by switch from the entries; sd-switch refuses a unit whose
            // value it does not know, and steward says it will not use it.
            "X-SwitchMethod" if SwitchMethod::parse(&e.value).is_none() => self.warn(
                e.line,
                format!(
                    "X-SwitchMethod={} is not reload, restart, stop-start or keep-old; ignored",
                    e.value
                ),
            ),
            _ => self.unknown(e, "Unit"),
        }
    }

    fn service_key(&mut self, s: &mut Service, e: &Entry) {
        match e.key.as_str() {
            "Type" => {
                s.service_type = match e.value.as_str() {
                    "simple" => ServiceType::Simple,
                    "exec" => ServiceType::Exec,
                    "forking" => ServiceType::Forking,
                    "oneshot" => ServiceType::Oneshot,
                    "notify" | "notify-reload" | "dbus" | "idle" => {
                        self.warn(
                            e.line,
                            format!("Type={} is not supported; treated as simple", e.value),
                        );
                        ServiceType::Simple
                    }
                    other => {
                        return self.error(e.line, format!("Type={other} is not a service type"))
                    }
                }
            }
            "ExecStart" => self.commands(e, &mut s.exec_start),
            "ExecStartPre" => self.commands(e, &mut s.exec_start_pre),
            "ExecStartPost" => self.commands(e, &mut s.exec_start_post),
            "ExecStop" => self.commands(e, &mut s.exec_stop),
            "ExecReload" => self.commands(e, &mut s.exec_reload),
            "Restart" => {
                s.restart = match e.value.as_str() {
                    "no" => Restart::No,
                    "on-success" => Restart::OnSuccess,
                    "on-failure" => Restart::OnFailure,
                    "on-abnormal" => Restart::OnAbnormal,
                    "always" => Restart::Always,
                    other => {
                        return self.error(e.line, format!("Restart={other} is not supported"))
                    }
                }
            }
            "RestartSec" => self.span(e, &mut s.restart_sec),
            "RestartSteps" => self.number(e, &mut s.restart_steps),
            "RestartMaxDelaySec" => self.span(e, &mut s.restart_max_delay),
            "TimeoutStartSec" => self.span(e, &mut s.timeout_start),
            "TimeoutStopSec" => self.span(e, &mut s.timeout_stop),
            // Where systemd before 230 had them; it still reads them here.
            "StartLimitBurst" => self.number(e, &mut s.start_limit_burst),
            "StartLimitIntervalSec" => self.span(e, &mut s.start_limit_interval),
            "TimeoutSec" => {
                if let Some(span) = self.timespan(e) {
                    s.timeout_start = span;
                    s.timeout_stop = span;
                }
            }
            "WorkingDirectory" => s.working_directory = non_empty(&e.value),
            "Environment" => self.environment(e, &mut s.environment),
            "KillMode" => {
                s.kill_mode = match e.value.as_str() {
                    "control-group" => KillMode::ControlGroup,
                    "process" => KillMode::Process,
                    "mixed" => {
                        self.warn(
                            e.line,
                            "KillMode=mixed is not supported; treated as control-group",
                        );
                        KillMode::ControlGroup
                    }
                    other => {
                        return self.error(e.line, format!("KillMode={other} is not supported"))
                    }
                }
            }
            "StandardOutput" => {
                s.standard_output = match e.value.as_str() {
                    // Empty is systemd's way of undoing an earlier line: the
                    // default again, whatever this machine's is.
                    "" => None,
                    "file" => Some(Output::File),
                    // `journal` is what a unit written for Linux says, and
                    // the Event Log is what it means here.
                    "eventlog" | "journal" => Some(Output::EventLog),
                    other => {
                        self.warn(
                            e.line,
                            format!("StandardOutput={other} is not supported; treated as file"),
                        );
                        Some(Output::File)
                    }
                }
            }
            _ => self.unknown(e, "Service"),
        }
    }

    fn timer_key(&mut self, t: &mut Timer, e: &Entry) {
        let trigger: fn(Duration) -> Trigger = match e.key.as_str() {
            // An empty assignment to any of them resets all of them, as in
            // systemd.
            "OnActiveSec" | "OnBootSec" | "OnStartupSec" | "OnUnitActiveSec"
            | "OnUnitInactiveSec" | "OnCalendar"
                if e.value.is_empty() =>
            {
                return t.triggers.clear();
            }
            "OnActiveSec" => Trigger::Active,
            "OnBootSec" => Trigger::Boot,
            "OnStartupSec" => Trigger::Startup,
            "OnUnitActiveSec" => Trigger::UnitActive,
            "OnUnitInactiveSec" => Trigger::UnitInactive,
            "OnCalendar" => {
                match parse_calendar(&e.value) {
                    Ok(calendar) => t.triggers.push(Trigger::Calendar(Box::new(calendar))),
                    Err(message) => {
                        self.bad_trigger = true;
                        self.error(e.line, format!("OnCalendar=: {message}"));
                    }
                }
                return;
            }
            "Unit" => {
                let unit = e.value.as_str();
                if unit.ends_with(".timer") {
                    self.error(
                        e.line,
                        "Unit= is a timer; a timer starts a service or a target",
                    );
                } else if BUILTIN_TARGETS.contains(&unit) {
                    self.error(
                        e.line,
                        format!(
                            "Unit={unit} is steward's own target, which is reached, not started"
                        ),
                    );
                } else if unit.ends_with(".service") || unit.ends_with(".target") {
                    t.unit = unit.to_owned();
                } else {
                    self.error(e.line, format!("Unit={unit} is not a .service or .target"));
                }
                return;
            }
            "Persistent" => return self.boolean(e, &mut t.persistent),
            "AccuracySec" => return self.span(e, &mut t.accuracy),
            "RandomizedDelaySec" => return self.span(e, &mut t.randomized_delay),
            "FixedRandomDelay" => return self.boolean(e, &mut t.fixed_random_delay),
            "RemainAfterElapse" => return self.boolean(e, &mut t.remain_after_elapse),
            "WakeSystem" => {
                let mut wake = false;
                self.boolean(e, &mut wake);
                if wake {
                    self.warn(
                        e.line,
                        "WakeSystem= is not supported; the timer elapses once the machine is awake",
                    );
                }
                return;
            }
            _ => return self.unknown(e, "Timer"),
        };
        match self.timespan(e) {
            Some(span) => t.triggers.push(trigger(span)),
            None => self.bad_trigger = true,
        }
    }

    fn install_key(&mut self, s: &mut Service, e: &Entry) {
        match e.key.as_str() {
            "WantedBy" => list(&mut s.wanted_by, &e.value),
            _ => self.unknown(e, "Install"),
        }
    }

    fn unknown(&mut self, e: &Entry, section: &str) {
        if !e.key.starts_with("X-") {
            self.warn(
                e.line,
                format!("{}= is not supported in [{section}]; ignored", e.key),
            );
        }
    }

    fn validate(&mut self, s: &Service) {
        if let Some(timer) = &s.timer {
            if timer.triggers.is_empty() && !self.bad_trigger {
                self.error(
                    0,
                    "no OnCalendar= or On...Sec= (a timer needs something to wait for)",
                );
            }
            let calendar = |t: &Trigger| matches!(t, Trigger::Calendar(_));
            if timer.persistent && !timer.triggers.iter().any(calendar) {
                self.warn(
                    0,
                    "Persistent= applies to OnCalendar= only, and there is none",
                );
            }
        }
        if s.runs_nothing() {
            return;
        }
        match (s.exec_start.len(), s.service_type) {
            _ if self.bad_exec_start => {}
            (0, _) => self.error(0, "no ExecStart= (a service needs a command to run)"),
            (1, _) | (_, ServiceType::Oneshot) => {}
            (n, _) => self.error(
                0,
                format!("{n} ExecStart= lines; only Type=oneshot may have more than one"),
            ),
        }
        if s.service_type == ServiceType::Oneshot
            && matches!(s.restart, Restart::Always | Restart::OnSuccess)
        {
            self.error(
                0,
                "Restart=always and Restart=on-success do not apply to Type=oneshot",
            );
        }
        if s.restart_steps > 0 && s.restart_max_delay < s.restart_sec {
            self.warn(
                0,
                "RestartMaxDelaySec= is shorter than RestartSec=; restarts wait RestartSec=",
            );
        }
        // With KillMode=process the job tracks the main process alone, and a
        // forking service's main process is the one that exits.
        if s.service_type == ServiceType::Forking && s.kill_mode == KillMode::Process {
            self.error(
                0,
                "Type=forking needs KillMode=control-group: the daemon it starts is otherwise not tracked",
            );
        }
    }

    fn commands(&mut self, e: &Entry, into: &mut Vec<Command>) {
        if e.value.is_empty() {
            into.clear();
            return;
        }
        let (line, ignore_failure) = match e.value.strip_prefix('-') {
            Some(rest) => (rest.trim_start(), true),
            None => (e.value.as_str(), false),
        };
        let problem = if let Some(prefix) = line
            .chars()
            .next()
            .filter(|c| matches!(c, '@' | '+' | '!' | ':'))
        {
            format!("the {prefix:?} prefix on {}= is not supported", e.key)
        } else if line.is_empty() {
            format!("{}= has a prefix but no command", e.key)
        } else {
            if let Some((program, rest)) = unquoted_program_with_a_space(line) {
                let first = line.split_whitespace().next().unwrap_or_default();
                self.warn(
                    e.line,
                    format!(
                        "{}= names a program whose path has a space, unquoted: Windows tries \
                         {first}.exe first. Quote it: \"{program}\"{rest}",
                        e.key
                    ),
                );
            }
            into.push(Command {
                line: line.to_owned(),
                ignore_failure,
            });
            return;
        };
        self.bad_exec_start |= e.key == "ExecStart";
        self.error(e.line, problem);
    }

    fn environment(&mut self, e: &Entry, into: &mut Vec<(String, String)>) {
        if e.value.is_empty() {
            into.clear();
            return;
        }
        let words = match split_quoted(&e.value) {
            Ok(words) => words,
            Err(message) => return self.error(e.line, message),
        };
        for word in words {
            match word.split_once('=') {
                Some((name, value)) if !name.is_empty() => {
                    into.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
                    into.push((name.to_owned(), value.to_owned()));
                }
                _ => self.error(
                    e.line,
                    format!("{word:?} in Environment= is not NAME=value"),
                ),
            }
        }
    }

    fn span(&mut self, e: &Entry, into: &mut Duration) {
        if let Some(span) = self.timespan(e) {
            *into = span;
        }
    }

    fn timespan(&mut self, e: &Entry) -> Option<Duration> {
        parse_timespan(&e.value)
            .map_err(|message| self.error(e.line, format!("{}=: {message}", e.key)))
            .ok()
    }

    /// systemd's booleans: 1, yes, y, true, t, on, and their opposites.
    fn boolean(&mut self, e: &Entry, into: &mut bool) {
        match e.value.to_ascii_lowercase().as_str() {
            "1" | "yes" | "y" | "true" | "t" | "on" => *into = true,
            "0" | "no" | "n" | "false" | "f" | "off" => *into = false,
            _ => self.error(e.line, format!("{}={} is not a boolean", e.key, e.value)),
        }
    }

    fn number(&mut self, e: &Entry, into: &mut u32) {
        match e.value.parse() {
            Ok(n) => *into = n,
            Err(_) => self.error(
                e.line,
                format!("{}={} is not a non-negative number", e.key, e.value),
            ),
        }
    }
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

/// Space-separated names; an empty assignment resets the list.
fn list(into: &mut Vec<String>, value: &str) {
    if value.is_empty() {
        into.clear();
    } else {
        into.extend(value.split_whitespace().map(str::to_owned));
    }
}

/// An unquoted command line whose program path has a space in it:
/// `C:\Program Files\whkd\whkd.exe --flag`. `CreateProcessW` with no
/// `lpApplicationName` takes the program to be the first whitespace-delimited
/// token and, failing that, each longer prefix in turn (`C:\Program.exe`,
/// `C:\Program Files\whkd\whkd.exe`, ...), so a stray `C:\Program.exe` runs
/// instead. The program is taken to end at the first token with a program
/// extension; the result is that program and the rest of the line (starting
/// with the space that separates them, or empty).
fn unquoted_program_with_a_space(line: &str) -> Option<(&str, &str)> {
    fn is_program(token: &str) -> bool {
        let lower = token.to_ascii_lowercase();
        [".exe", ".com", ".bat", ".cmd"]
            .iter()
            .any(|ext| lower.ends_with(ext))
    }
    let first = line.split_whitespace().next()?;
    // A quoted program, a bare name found on PATH, or a path that already
    // names the program: none of these misroute.
    if first.starts_with('"') || !first.contains(['\\', '/']) || is_program(first) {
        return None;
    }
    let end = line.split_whitespace().skip(1).find(|t| is_program(t))?;
    let offset = end.as_ptr() as usize - line.as_ptr() as usize + end.len();
    Some(line.split_at(offset))
}

/// Words separated by whitespace; `"` or `'` group, and are removed.
/// Backslashes are ordinary characters (they are path separators here).
fn split_quoted(value: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in value.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if let Some(q) = quote {
        return Err(format!("unterminated {q} in {value:?}"));
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> Service {
        let parsed = parse_service("test.service", text);
        assert!(
            !parsed.has_errors(),
            "unexpected errors: {:?}",
            parsed.diagnostics
        );
        parsed.service.unwrap()
    }

    fn messages(text: &str) -> Vec<String> {
        parse_service("test.service", text)
            .diagnostics
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn a_desktop_daemon() {
        let s = ok(r#"
[Unit]
Description=Hotkey daemon
After=graphical-session.target

[Service]
ExecStart="C:\Program Files\whkd\bin\whkd.exe"
Restart=on-failure
RestartSec=2s
KillMode=process
Environment=WHKD_CONFIG_HOME=%USERPROFILE%\.config "GREETING=hello world"

[Install]
WantedBy=graphical-session.target
"#);
        assert_eq!(s.name, "test.service");
        assert_eq!(s.description.as_deref(), Some("Hotkey daemon"));
        assert_eq!(s.after, ["graphical-session.target"]);
        assert_eq!(
            s.exec_start,
            [Command {
                line: r#""C:\Program Files\whkd\bin\whkd.exe""#.into(),
                ignore_failure: false
            }]
        );
        assert_eq!(s.restart, Restart::OnFailure);
        assert_eq!(s.restart_sec, Duration::from_secs(2));
        assert_eq!(s.kill_mode, KillMode::Process);
        assert_eq!(
            s.environment,
            [
                ("WHKD_CONFIG_HOME".into(), r"%USERPROFILE%\.config".into()),
                ("GREETING".into(), "hello world".into())
            ]
        );
        assert_eq!(s.wanted_by, ["graphical-session.target"]);
    }

    #[test]
    fn defaults() {
        let s = ok("[Service]\nExecStart=x.exe\n");
        assert_eq!(s.service_type, ServiceType::Simple);
        // Durability first: a crash is restarted, with backoff.
        assert_eq!(s.restart, Restart::OnFailure);
        assert_eq!(s.restart_sec, Duration::from_secs(1));
        assert_eq!(
            (s.restart_steps, s.restart_max_delay),
            (5, Duration::from_secs(60))
        );
        assert_eq!(s.timeout_start, Duration::from_secs(30));
        assert_eq!(s.timeout_stop, Duration::from_secs(10));
        assert_eq!(s.kill_mode, KillMode::ControlGroup);
        assert_eq!(
            (s.start_limit_burst, s.start_limit_interval),
            (5, Duration::from_secs(10))
        );
        assert_eq!(s.working_directory, None);
    }

    #[test]
    fn lists_accumulate_and_an_empty_assignment_resets() {
        let s = ok(
            "[Unit]\nAfter=a.service b.service\nAfter=c.service\nWants=x.service\nWants=\n\
                    [Service]\nExecStart=x.exe\nExecStop=one.exe\nExecStop=\nExecStop=two.exe\n\
                    Environment=A=1 B=2\nEnvironment=\nEnvironment=C=3\n",
        );
        assert_eq!(s.after, ["a.service", "b.service", "c.service"]);
        assert!(s.wants.is_empty());
        assert_eq!(s.exec_stop.len(), 1);
        assert_eq!(s.exec_stop[0].line, "two.exe");
        assert_eq!(s.environment, [("C".into(), "3".into())]);
    }

    /// `ExecReload=` is a list of commands like `ExecStop=`: the `-` prefix,
    /// the others refused, and an empty assignment starting it over.
    #[test]
    fn exec_reload() {
        let s = ok(
            "[Service]\nExecStart=x.exe\nExecReload=old.exe\nExecReload=\n\
                    ExecReload=x.exe --reload\nExecReload=-notify.exe\n",
        );
        assert_eq!(
            s.exec_reload,
            [
                Command {
                    line: "x.exe --reload".into(),
                    ignore_failure: false
                },
                Command {
                    line: "notify.exe".into(),
                    ignore_failure: true
                }
            ]
        );
        assert!(ok("[Service]\nExecStart=x.exe\n").exec_reload.is_empty());
        assert_eq!(
            messages("[Service]\nExecStart=x.exe\nExecReload=+x.exe --reload\n"),
            ["line 3: error: the '+' prefix on ExecReload= is not supported"]
        );
        assert_eq!(
            messages("[Service]\nExecStart=x.exe\nExecReload=C:\\My Tools\\r.exe\n"),
            [
                "line 3: warning: ExecReload= names a program whose path has a space, unquoted: \
                 Windows tries C:\\My.exe first. Quote it: \"C:\\My Tools\\r.exe\""
            ]
        );
    }

    /// The file's entries are kept as written, the keys the parser skips
    /// among them, for `switch` to compare.
    #[test]
    fn the_entries_keep_what_the_parser_skips() {
        let text = "[Unit]\nX-Restart-Triggers=abc\n[Service]\nExecStart=x.exe\n";
        let s = ok(text);
        assert_eq!(
            s.entries,
            crate::compare::Entries::of(&syntax::parse(text).unwrap())
        );
        // Two files that mean the same differ in their entries all the same.
        assert_ne!(s, ok("[Service]\nExecStart=x.exe\n"));
    }

    #[test]
    fn a_later_environment_value_replaces_an_earlier_one() {
        // Windows variable names are case-insensitive.
        let s = ok("[Service]\nExecStart=x.exe\nEnvironment=Path=a\nEnvironment=PATH=b\n");
        assert_eq!(s.environment, [("PATH".into(), "b".into())]);
    }

    #[test]
    fn scalars_take_the_last_value() {
        let s = ok("[Service]\nExecStart=x.exe\nRestart=always\nRestart=on-abnormal\nWorkingDirectory=C:\\a\nWorkingDirectory=\n");
        assert_eq!(s.restart, Restart::OnAbnormal);
        assert_eq!(s.working_directory, None);
    }

    #[test]
    fn exec_start_can_be_reset_and_replaced() {
        let s = ok("[Service]\nExecStart=old.exe\nExecStart=\nExecStart=new.exe\n");
        assert_eq!(s.exec_start.len(), 1);
        assert_eq!(s.exec_start[0].line, "new.exe");
    }

    #[test]
    fn the_ignore_failure_prefix() {
        let s = ok("[Service]\nExecStart=x.exe\nExecStop=-komorebic.exe stop\n");
        assert_eq!(
            s.exec_stop,
            [Command {
                line: "komorebic.exe stop".into(),
                ignore_failure: true
            }]
        );
    }

    #[test]
    fn an_unquoted_program_path_with_a_space_warns() {
        assert_eq!(
            messages("[Service]\nExecStart=C:\\Program Files\\whkd\\whkd.exe --flag\n"),
            [
                "line 2: warning: ExecStart= names a program whose path has a space, unquoted: \
                 Windows tries C:\\Program.exe first. Quote it: \"C:\\Program Files\\whkd\\whkd.exe\" --flag"
            ]
        );
        // The key is named and the prefix is not part of it; a program ends
        // at its extension, whatever the case, with or without arguments.
        assert_eq!(
            messages("[Service]\nExecStart=x\nExecStop=-%LOCALAPPDATA%/Programs/My App/app.CMD\n"),
            [
                "line 3: warning: ExecStop= names a program whose path has a space, unquoted: \
                 Windows tries %LOCALAPPDATA%/Programs/My.exe first. \
                 Quote it: \"%LOCALAPPDATA%/Programs/My App/app.CMD\""
            ]
        );
        // The command still loads, as written.
        let s = ok("[Service]\nExecStart=C:\\Program Files\\a b\\c.exe d\n");
        assert_eq!(s.exec_start[0].line, r"C:\Program Files\a b\c.exe d");
    }

    #[test]
    fn programs_that_do_not_misroute_are_quiet() {
        for line in [
            r#""C:\Program Files\whkd\bin\whkd.exe" --flag"#,
            r"C:\tools\whkd.exe --flag",
            r"C:\tools\whkd --flag",
            "komorebic stop",
            "whkd.exe",
            r"restic backup C:\Users\alice\My Documents",
            r"python C:\my scripts\build.cmd",
            r#"cmd.exe /d /c "C:\Program Files\x\y.exe""#,
        ] {
            assert!(
                messages(&format!("[Service]\nExecStart={line}\n")).is_empty(),
                "{line}"
            );
        }
    }

    #[test]
    fn other_prefixes_are_errors() {
        assert_eq!(
            messages("[Service]\nExecStart=+x.exe\n"),
            ["line 2: error: the '+' prefix on ExecStart= is not supported"]
        );
        assert!(parse_service("t.service", "[Service]\nExecStart=-\n").has_errors());
    }

    #[test]
    fn a_service_needs_exactly_one_command_unless_oneshot() {
        assert_eq!(
            messages("[Unit]\nDescription=x\n"),
            ["error: no ExecStart= (a service needs a command to run)"]
        );
        assert!(parse_service("t.service", "[Service]\nExecStart=a\nExecStart=b\n").has_errors());
        let s = ok("[Service]\nType=oneshot\nExecStart=a\nExecStart=b\n");
        assert_eq!(s.exec_start.len(), 2);
        assert!(parse_service(
            "t.service",
            "[Service]\nType=oneshot\nExecStart=a\nRestart=always\n"
        )
        .has_errors());
    }

    #[test]
    fn unknown_things_warn_and_x_things_are_silent() {
        let parsed = parse_service(
            "t.service",
            "[Unit]\nConditionPathExists=/x\nX-Mine=1\n[Service]\nExecStart=x.exe\nNice=5\n[X-Custom]\nA=b\n[Timer]\nOnCalendar=daily\n",
        );
        assert!(!parsed.has_errors());
        let got: Vec<String> = parsed.diagnostics.iter().map(ToString::to_string).collect();
        assert_eq!(
            got,
            [
                "line 2: warning: ConditionPathExists= is not supported in [Unit]; ignored",
                "line 6: warning: Nice= is not supported in [Service]; ignored",
                "line 9: warning: section [Timer] is not supported; ignored",
            ]
        );
    }

    #[test]
    fn a_switch_method_steward_does_not_know_warns() {
        let warnings = |value: &str| -> Vec<String> {
            parse_service(
                "t.service",
                &format!("[Unit]\nX-SwitchMethod={value}\n[Service]\nExecStart=x.exe\n"),
            )
            .diagnostics
            .iter()
            .map(ToString::to_string)
            .collect()
        };
        for known in ["reload", "restart", "stop-start", "keep-old"] {
            assert!(warnings(known).is_empty(), "{known}");
        }
        assert_eq!(
            warnings("stop-only"),
            ["line 2: warning: X-SwitchMethod=stop-only is not reload, restart, stop-start or keep-old; ignored"]
        );
    }

    /// `StandardOutput=eventlog` sends output to the user's channel, and
    /// `journal` is the same choice under its Linux name; `file` and
    /// anything this cannot do mean the file, the last with a warning.
    /// Unset, or set to nothing, leaves it to the machine.
    #[test]
    fn where_the_output_goes() {
        let with = |value: &str| format!("[Service]\nExecStart=x.exe\nStandardOutput={value}\n");
        assert_eq!(ok("[Service]\nExecStart=x.exe\n").standard_output, None);
        assert_eq!(ok(&with("")).standard_output, None);
        assert_eq!(
            ok(&with("eventlog")).standard_output,
            Some(Output::EventLog)
        );
        assert_eq!(ok(&with("journal")).standard_output, Some(Output::EventLog));
        assert_eq!(ok(&with("file")).standard_output, Some(Output::File));
        assert_eq!(
            ok(&(with("eventlog") + "StandardOutput=file\n")).standard_output,
            Some(Output::File)
        );
        assert_eq!(
            ok(&(with("file") + "StandardOutput=\n")).standard_output,
            None
        );
        let parsed = parse_service("t.service", &with("null"));
        assert!(!parsed.has_errors());
        assert_eq!(parsed.service.unwrap().standard_output, Some(Output::File));
        assert_eq!(
            messages(&with("null")),
            ["line 3: warning: StandardOutput=null is not supported; treated as file"]
        );
    }

    #[test]
    fn linux_service_types_load_as_simple_with_a_warning() {
        let parsed = parse_service("t.service", "[Service]\nType=notify\nExecStart=x.exe\n");
        assert!(!parsed.has_errors());
        assert_eq!(parsed.service.unwrap().service_type, ServiceType::Simple);
        assert_eq!(parsed.diagnostics.len(), 1);
    }

    #[test]
    fn bad_values_are_errors_with_lines() {
        assert_eq!(
            messages("[Service]\nExecStart=x\nRestartSec=soon\n"),
            ["line 3: error: RestartSec=: \"soon\" is not a time span"]
        );
        assert_eq!(
            messages("[Service]\nExecStart=x\nRestart=sometimes\n"),
            ["line 3: error: Restart=sometimes is not supported"]
        );
        assert_eq!(
            messages("[Unit]\nStartLimitBurst=-1\n[Service]\nExecStart=x\n"),
            ["line 2: error: StartLimitBurst=-1 is not a non-negative number"]
        );
        assert_eq!(
            messages("[Service]\nExecStart=x\nEnvironment=NOEQUALS\n"),
            ["line 3: error: \"NOEQUALS\" in Environment= is not NAME=value"]
        );
        assert_eq!(
            messages("[Service]\nExecStart=x\nEnvironment=\"A=1\n"),
            ["line 3: error: unterminated \" in \"\\\"A=1\""]
        );
    }

    #[test]
    fn syntax_errors_come_through_as_diagnostics() {
        let parsed = parse_service("t.service", "[Service]\nExecStart=x\ngarbage\n");
        assert!(parsed.service.is_none());
        assert_eq!(parsed.diagnostics[0].line, 3);
    }

    #[test]
    fn the_name_must_be_a_service_a_target_or_a_timer() {
        assert_eq!(
            parse_service("t.socket", "[Service]\nExecStart=x\n").diagnostics[0].to_string(),
            "error: \"t.socket\" is not a .service, .target or .timer unit"
        );
    }

    fn timer(text: &str) -> Timer {
        let parsed = parse_service("backup.timer", text);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let unit = parsed.service.unwrap();
        assert!(unit.is_timer() && unit.runs_nothing());
        unit.timer.unwrap()
    }

    fn timer_messages(text: &str) -> Vec<String> {
        parse_service("backup.timer", text)
            .diagnostics
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn a_timer_starts_the_service_named_as_it_is_by_default() {
        let t = timer("[Unit]\nDescription=Nightly\n[Timer]\nOnCalendar=daily\nPersistent=true\n[Install]\nWantedBy=timers.target\n");
        assert_eq!(t.unit, "backup.service");
        assert!(t.persistent);
        assert!(t.remain_after_elapse);
        assert_eq!(t.randomized_delay, Duration::ZERO);
        assert!(matches!(&t.triggers[..], [Trigger::Calendar(c)] if c.to_string() == "daily"));
        assert_eq!(
            timer("[Timer]\nOnBootSec=1\nUnit=other.target\n").unit,
            "other.target"
        );
    }

    #[test]
    fn a_timer_s_triggers_accumulate_and_an_empty_one_resets_them_all() {
        let t = timer("[Timer]\nOnCalendar=hourly\nOnBootSec=5min\nOnUnitActiveSec=1h\nOnUnitInactiveSec=30s\nOnStartupSec=0\nOnActiveSec=1s\n");
        assert_eq!(
            t.triggers[1..],
            [
                Trigger::Boot(Duration::from_secs(300)),
                Trigger::UnitActive(Duration::from_secs(3600)),
                Trigger::UnitInactive(Duration::from_secs(30)),
                Trigger::Startup(Duration::ZERO),
                Trigger::Active(Duration::from_secs(1)),
            ]
        );
        let t =
            timer("[Timer]\nOnCalendar=hourly\nOnBootSec=5min\nOnActiveSec=\nOnUnitActiveSec=1h\n");
        assert_eq!(t.triggers, [Trigger::UnitActive(Duration::from_secs(3600))]);
    }

    #[test]
    fn timer_settings() {
        let t = timer("[Timer]\nOnActiveSec=1\nRandomizedDelaySec=10min\nFixedRandomDelay=yes\nRemainAfterElapse=no\nAccuracySec=1s\nWakeSystem=false\n");
        assert_eq!(t.randomized_delay, Duration::from_secs(600));
        assert!(t.fixed_random_delay);
        assert!(!t.remain_after_elapse);
        assert_eq!(t.accuracy, Duration::from_secs(1));
    }

    #[test]
    fn what_a_timer_cannot_be() {
        assert_eq!(
            timer_messages("[Unit]\nDescription=x\n"),
            ["error: no OnCalendar= or On...Sec= (a timer needs something to wait for)"]
        );
        assert_eq!(
            timer_messages("[Timer]\nOnCalendar=daily\n[Service]\nExecStart=x\n"),
            ["line 3: error: a timer runs nothing itself; it has no [Service] section (Unit= names what it starts)"]
        );
        assert_eq!(
            timer_messages("[Timer]\nOnCalendar=sometimes\n"),
            ["line 2: error: OnCalendar=: \"sometimes\" is not a day of the week"]
        );
        assert_eq!(
            timer_messages("[Timer]\nOnBootSec=soon\n"),
            ["line 2: error: OnBootSec=: \"soon\" is not a time span"]
        );
        assert_eq!(
            timer_messages("[Timer]\nOnBootSec=1\nUnit=other.timer\n"),
            ["line 3: error: Unit= is a timer; a timer starts a service or a target"]
        );
        assert_eq!(
            timer_messages("[Timer]\nOnBootSec=1\nUnit=other.socket\n"),
            ["line 3: error: Unit=other.socket is not a .service or .target"]
        );
        assert_eq!(
            timer_messages("[Timer]\nOnBootSec=1\nPersistent=maybe\n"),
            ["line 3: error: Persistent=maybe is not a boolean"]
        );
    }

    #[test]
    fn what_a_timer_warns_about() {
        assert_eq!(
            timer_messages("[Timer]\nOnBootSec=1\nPersistent=true\n"),
            ["warning: Persistent= applies to OnCalendar= only, and there is none"]
        );
        assert_eq!(
            timer_messages("[Timer]\nOnBootSec=1\nWakeSystem=true\nOnClockChange=yes\n"),
            [
                "line 3: warning: WakeSystem= is not supported; the timer elapses once the machine is awake",
                "line 4: warning: OnClockChange= is not supported in [Timer]; ignored",
            ]
        );
    }

    #[test]
    fn a_target_is_unit_and_install_only() {
        let parsed = parse_service(
            "tiling.target",
            "[Unit]\nDescription=Tiling\nWants=komorebi.service whkd.service\nAfter=graphical-session.target\n[Install]\nWantedBy=graphical-session.target\n",
        );
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let t = parsed.service.unwrap();
        assert!(t.is_target());
        assert_eq!(t.wants, ["komorebi.service", "whkd.service"]);
        assert_eq!(t.wanted_by, ["graphical-session.target"]);
        assert!(t.exec_start.is_empty());

        let with_service = parse_service("t.target", "[Service]\nExecStart=x\n");
        assert_eq!(
            with_service.diagnostics[0].to_string(),
            "line 1: error: a target runs nothing; it has no [Service] section"
        );
    }

    #[test]
    fn steward_s_own_targets_are_not_files() {
        for name in BUILTIN_TARGETS {
            assert!(parse_service(name, "[Unit]\n").has_errors(), "{name}");
        }
    }

    #[test]
    fn part_of() {
        let s = ok("[Unit]\nPartOf=tiling.target\n[Service]\nExecStart=x\n");
        assert_eq!(s.part_of, ["tiling.target"]);
        assert!(!s.is_target());
    }

    #[test]
    fn backoff() {
        let s = ok("[Service]\nExecStart=x\nRestart=always\nRestartSec=1s\nRestartSteps=5\nRestartMaxDelaySec=1min\n");
        assert_eq!(
            (s.restart_steps, s.restart_max_delay),
            (5, Duration::from_secs(60))
        );
        assert_eq!(
            messages("[Service]\nExecStart=x\nRestartSec=2min\n"),
            ["warning: RestartMaxDelaySec= is shorter than RestartSec=; restarts wait RestartSec="]
        );
        assert!(messages("[Service]\nExecStart=x\nRestartSec=2min\nRestartSteps=0\n").is_empty());
    }

    #[test]
    fn the_start_limit_can_be_in_either_section() {
        let s =
            ok("[Unit]\nStartLimitBurst=2\n[Service]\nExecStart=x\nStartLimitIntervalSec=1min\n");
        assert_eq!(
            (s.start_limit_burst, s.start_limit_interval),
            (2, Duration::from_secs(60))
        );
    }

    #[test]
    fn timeouts() {
        let s = ok("[Service]\nExecStart=x\nTimeoutSec=5s\nTimeoutStopSec=3s\n");
        assert_eq!(s.timeout_start, Duration::from_secs(5));
        assert_eq!(s.timeout_stop, Duration::from_secs(3));
    }

    #[test]
    fn forking_needs_the_whole_tree() {
        assert_eq!(
            messages("[Service]\nType=forking\nKillMode=process\nExecStart=x\n"),
            ["error: Type=forking needs KillMode=control-group: the daemon it starts is otherwise not tracked"]
        );
    }
}
