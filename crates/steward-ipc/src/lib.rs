//! How `stewctl` talks to `steward`: one request, one response, each a line
//! of JSON, over the named pipe `\\.\pipe\steward-<user SID>-<session>` (see
//! [`pipe`]).
//!
//! The pipe admits only the user who owns it, refuses remote clients, and the
//! client checks that the process serving it runs as that user before it
//! believes a word -- a pipe name is not a secret, and another account could
//! create one first.

use serde::{Deserialize, Serialize};

#[cfg(windows)]
pub mod pipe;

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
    /// Read the unit files again. A removed unit is stopped; a changed one
    /// keeps running with its old definition until it is restarted. With
    /// `apply`, the running set is made to match the files: changed units
    /// that run are restarted, and new units, newly wanted ones and failed
    /// ones that changed are started. A unit stopped on purpose stays stopped.
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
}

impl UnitStatus {
    pub fn is_active(&self) -> bool {
        self.state == "active"
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
    }

    #[test]
    fn a_bare_error_is_a_response() {
        let response: Response = serde_json::from_str(r#"{"error":"no such unit"}"#).unwrap();
        assert_eq!(response, Response::error("no such unit"));
    }
}
