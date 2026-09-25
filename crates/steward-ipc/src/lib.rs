//! How `stewardctl` talks to `steward`: one request, one response, each a line
//! of JSON, over the named pipe `\\.\pipe\steward-<user SID>-<session>` (see
//! [`pipe`]).
//!
//! The pipe admits only the user who owns it, refuses remote clients, and the
//! client checks that it was created by that user before it believes a word
//! -- a pipe name is not a secret, and another account could create one
//! first.

use serde::{Deserialize, Serialize};

#[cfg(windows)]
pub mod channel;
#[cfg(windows)]
pub mod pipe;

/// [`UnitStatus::output`] for a unit whose output goes to the user's Event
/// Log channel.
pub const OUTPUT_EVENTLOG: &str = "eventlog";
/// [`UnitStatus::output`] for a unit whose output goes to its log file.
pub const OUTPUT_FILE: &str = "file";

/// The most a request or response may be, in bytes.
pub const MAX_MESSAGE: u64 = 4 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "kebab-case")]
pub enum Request {
    /// The manager and the named units (all of them if none are named).
    Status {
        units: Vec<String>,
    },
    /// Start these units, and what they want or require.
    Start {
        units: Vec<String>,
    },
    Stop {
        units: Vec<String>,
    },
    Restart {
        units: Vec<String>,
    },
    /// Run these units' `ExecReload=` while they keep running. Refused,
    /// doing nothing, unless every one of them is an active service that
    /// has `ExecReload=`.
    ReloadUnits {
        units: Vec<String>,
    },
    /// Reload the units that can be reloaded -- active services with
    /// `ExecReload=` -- and restart the rest; with `only_running`, leave
    /// those at rest alone rather than start them.
    ReloadOrRestart {
        units: Vec<String>,
        #[serde(default)]
        only_running: bool,
    },
    /// Read the unit files again. A removed unit is stopped; a changed one
    /// keeps running with its old definition until it is restarted, or, if
    /// the change is only in what a reload runs or is triggered by, takes
    /// the new one at once, to be reloaded. With `apply`, the running set is
    /// made to match the files: changed units that run are restarted or
    /// reloaded, and new units, newly wanted ones and failed ones that
    /// changed are started. A unit stopped on purpose stays stopped.
    Reload {
        apply: bool,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Response {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What the request did, a line each.
    #[serde(default)]
    pub messages: Vec<String>,
    #[serde(default)]
    pub manager: Option<ManagerStatus>,
    #[serde(default)]
    pub units: Vec<UnitStatus>,
}

impl Response {
    pub fn error(message: impl Into<String>) -> Response {
        Response {
            error: Some(message.into()),
            ..Response::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagerStatus {
    pub version: String,
    pub pid: u32,
    /// The session it manages.
    #[serde(default)]
    pub session: u32,
    /// Whether the shell is ready (`graphical-session.target` reached).
    pub graphical_session: bool,
    /// Whether the tray takes icons (`tray.target` reached).
    #[serde(default)]
    pub tray: bool,
    pub unit_dir: String,
    pub log_dir: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnitStatus {
    pub name: String,
    pub description: Option<String>,
    pub path: String,
    /// The state machine's state: `active`, `inactive`, `failed`,
    /// `auto-restart`, `start`, `stop-asked`, ...
    pub state: String,
    pub main_pid: Option<u32>,
    /// Every process in the service's job.
    pub pids: Vec<u32>,
    pub restarts: u32,
    /// How it last ended, in words.
    pub last_outcome: Option<String>,
    /// When it entered its state, local time.
    pub since: Option<String>,
    pub for_secs: Option<u64>,
    /// Seconds until an automatic restart, when one is due.
    pub restart_in_secs: Option<f64>,
    pub wanted_by: Vec<String>,
    /// Its unit file changed and it has not been restarted since.
    pub changed: bool,
    /// Its unit file changed only in what a reload runs or is triggered by,
    /// and it has not been reloaded (or restarted) since.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reload_due: bool,
    /// How its last reload since it started ended, in words: `exited
    /// cleanly` if every `ExecReload=` command did. Absent if it has not
    /// been reloaded, or a reload is running or was cut short.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reload: Option<String>,
    /// A timer's schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timer: Option<TimerStatus>,
    /// Set for a unit whose output was to go to the Event Log channel but goes
    /// to its log file for this run instead, and why: its `steward-cat` could
    /// not be started, or exited with the unit still running. `stewardctl` reads
    /// the file rather than the channel while this is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_fallback: Option<String>,
    /// Where its output goes, [`OUTPUT_EVENTLOG`] or [`OUTPUT_FILE`]: what its
    /// unit file says, or, where it says nothing, what the manager found on
    /// this machine -- the channel if there is one, or if the provisioning
    /// task could be run to make it, and the file otherwise. `stewardctl` reads
    /// this rather than deciding again. Absent from a manager from before the
    /// Event Log was the default, which only ever used what the file said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimerStatus {
    /// What it starts.
    pub unit: String,
    /// `waiting` for its next elapse, `running` (it elapsed, and its unit has
    /// not come to rest yet), or `elapsed` (nothing more is due); empty while
    /// the timer is not active.
    pub state: String,
    /// When it next elapses, local time.
    pub next: Option<String>,
    /// How long until then; negative when it is due.
    pub next_in_secs: Option<f64>,
    /// When it last elapsed, local time.
    pub last: Option<String>,
    pub last_secs_ago: Option<u64>,
}

impl UnitStatus {
    /// Up: active, or active and reloading.
    pub fn is_active(&self) -> bool {
        matches!(self.state.as_str(), "active" | "reload")
    }

    /// Still on its way up from a start.
    pub fn is_starting(&self) -> bool {
        matches!(self.state.as_str(), "start-pre" | "start" | "start-post")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_tagged_json() {
        let request = Request::Start {
            units: vec!["whkd.service".into()],
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(json, r#"{"request":"start","units":["whkd.service"]}"#);
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
        let reload: Request = serde_json::from_str(r#"{"request":"reload","apply":true}"#).unwrap();
        assert_eq!(reload, Request::Reload { apply: true });
        // A unit's reload is not the manager's.
        let units = Request::ReloadUnits {
            units: vec!["whkd.service".into()],
        };
        assert_eq!(
            serde_json::to_string(&units).unwrap(),
            r#"{"request":"reload-units","units":["whkd.service"]}"#
        );
        let either: Request =
            serde_json::from_str(r#"{"request":"reload-or-restart","units":["a.service"]}"#)
                .unwrap();
        assert_eq!(
            either,
            Request::ReloadOrRestart {
                units: vec!["a.service".into()],
                only_running: false
            }
        );
    }

    #[test]
    fn a_bare_error_is_a_response() {
        let response: Response = serde_json::from_str(r#"{"error":"no such unit"}"#).unwrap();
        assert_eq!(response, Response::error("no such unit"));
    }
}
