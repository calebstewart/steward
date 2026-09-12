//! A `.service` unit: the keys steward understands, their defaults, and a
//! diagnostic for everything else.
//!
//! Assignment follows systemd: a scalar key takes its last value; a list key
//! (`After=`, `Environment=`, `ExecStartPre=`, ...) accumulates, and an empty
//! assignment resets it. Keys and sections steward does not know are warnings,
//! so a unit written for Linux still loads and says what it ignored; `X-`
//! sections and keys are ignored silently, as systemd does.
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

use crate::syntax::{self, Entry, UnitFile};
use crate::time::parse_timespan;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// A Windows command line, as written.
    pub line: String,
    /// `-` prefix: a failure of this command does not fail the service.
    pub ignore_failure: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// The unit's name, `whkd.service`.
    pub name: String,
    pub description: Option<String>,
    pub documentation: Vec<String>,
    pub after: Vec<String>,
    pub before: Vec<String>,
    pub wants: Vec<String>,
    pub requires: Vec<String>,
    pub start_limit_burst: u32,
    pub start_limit_interval: Duration,

    pub service_type: ServiceType,
    pub exec_start: Vec<Command>,
    pub exec_start_pre: Vec<Command>,
    pub exec_start_post: Vec<Command>,
    pub exec_stop: Vec<Command>,
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

    pub wanted_by: Vec<String>,
}

impl Service {
    fn new(name: &str) -> Self {
        Service {
            name: name.to_owned(),
            description: None,
            documentation: Vec::new(),
            after: Vec::new(),
            before: Vec::new(),
            wants: Vec::new(),
            requires: Vec::new(),
            start_limit_burst: 5,
            start_limit_interval: Duration::from_secs(10),
            service_type: ServiceType::Simple,
            exec_start: Vec::new(),
            exec_start_pre: Vec::new(),
            exec_start_post: Vec::new(),
            exec_stop: Vec::new(),
            restart: Restart::OnFailure,
            restart_sec: Duration::from_secs(1),
            restart_steps: 5,
            restart_max_delay: Duration::from_secs(60),
            timeout_start: Duration::from_secs(30),
            timeout_stop: Duration::from_secs(10),
            working_directory: None,
            environment: Vec::new(),
            kill_mode: KillMode::ControlGroup,
            wanted_by: Vec::new(),
        }
    }
}

/// The result of reading a unit: the service, unless a diagnostic is an error.
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

/// Read `text` as the unit called `name` (its file name, `whkd.service`).
pub fn parse_service(name: &str, text: &str) -> Parsed {
    let mut reader = Reader {
        diagnostics: Vec::new(),
        bad_exec_start: false,
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
        if !name.ends_with(".service") {
            self.error(0, format!("{name:?} is not a .service unit"));
        }
        let mut s = Service::new(name);
        for section in &file.sections {
            match section.name.as_str() {
                "Unit" => section
                    .entries
                    .iter()
                    .for_each(|e| self.unit_key(&mut s, e)),
                "Service" => section
                    .entries
                    .iter()
                    .for_each(|e| self.service_key(&mut s, e)),
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
            "StartLimitBurst" => self.number(e, &mut s.start_limit_burst),
            "StartLimitIntervalSec" => self.span(e, &mut s.start_limit_interval),
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
            _ => self.unknown(e, "Service"),
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
    fn the_name_must_be_a_service() {
        assert!(parse_service("t.timer", "[Service]\nExecStart=x\n").has_errors());
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
