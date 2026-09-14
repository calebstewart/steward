//! What a restarted manager needs to take its services back: for each running
//! service, every process in its job and which of them is the main one;
//! which targets were active; each active timer's schedule; and the units
//! left at rest, and what put them there, so that what was stopped, finished,
//! failed or spent stays so. Written on every change to
//! `%LOCALAPPDATA%\steward\state-<session>.json` (a temporary file renamed
//! over the old, so a crash mid-write leaves the previous state rather than
//! half of one), and removed when there is nothing to record or everything
//! has been stopped. One per session, as managers are: a session's services
//! are its own manager's to adopt. Session numbers are reused, so the file
//! also says which sign-in it is from.
//!
//! Also here: the stamps persistent timers leave, one per timer, in
//! `%LOCALAPPDATA%\steward\timers\<unit>`. Those are the user's, not the
//! session's, and outlive it: they are how a timer knows at sign-in what it
//! missed while the user was away.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use steward_supervisor::Outcome;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    /// When the session's user signed in, in milliseconds since 1970, UTC:
    /// the sign-in the rest of the file belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logon: Option<u64>,
    pub units: BTreeMap<String, SavedUnit>,
    /// The targets that were active. They have no processes to find again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    /// The timers that were active, and where each was in its schedule.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub timers: BTreeMap<String, SavedTimer>,
    /// The units at rest that had run, or been refused, in this sign-in.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rests: BTreeMap<String, SavedRest>,
}

impl Saved {
    /// Whether this is the state of the sign-in that began at `logon`. One
    /// that does not say is from before sign-ins were recorded, and is taken
    /// as this one's, as it was then.
    pub fn same_sign_in(&self, logon: Option<u64>) -> bool {
        self.logon.is_none() || self.logon == logon
    }

    /// Nothing runs and nothing is at rest: there is nothing to record.
    fn is_empty(&self) -> bool {
        self.units.is_empty()
            && self.targets.is_empty()
            && self.timers.is_empty()
            && self.rests.is_empty()
    }
}

/// A unit at rest, and what put it there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedRest {
    pub state: RestState,
    /// How it last ended, if it is a service that ran or a unit that was
    /// refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<SavedOutcome>,
    /// A timer's last elapse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_trigger: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestState {
    /// Stopped on purpose, finished, or (a timer) spent: it stays at rest.
    Inactive,
    /// It stays failed, as the last manager concluded.
    Failed,
    /// Waiting out a restart delay: the next manager owes it the restart.
    AutoRestart,
}

/// [`Outcome`], as the file spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SavedOutcome {
    Clean,
    ExitCode(u32),
    Interrupted,
    Crashed(u32),
    Vanished,
    Timeout,
    SpawnFailed,
    StartLimit,
    Dependency,
}

impl From<Outcome> for SavedOutcome {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Clean => SavedOutcome::Clean,
            Outcome::ExitCode(code) => SavedOutcome::ExitCode(code),
            Outcome::Interrupted => SavedOutcome::Interrupted,
            Outcome::Crashed(code) => SavedOutcome::Crashed(code),
            Outcome::Vanished => SavedOutcome::Vanished,
            Outcome::Timeout => SavedOutcome::Timeout,
            Outcome::SpawnFailed => SavedOutcome::SpawnFailed,
            Outcome::StartLimit => SavedOutcome::StartLimit,
            Outcome::Dependency => SavedOutcome::Dependency,
        }
    }
}

impl From<SavedOutcome> for Outcome {
    fn from(outcome: SavedOutcome) -> Self {
        match outcome {
            SavedOutcome::Clean => Outcome::Clean,
            SavedOutcome::ExitCode(code) => Outcome::ExitCode(code),
            SavedOutcome::Interrupted => Outcome::Interrupted,
            SavedOutcome::Crashed(code) => Outcome::Crashed(code),
            SavedOutcome::Vanished => Outcome::Vanished,
            SavedOutcome::Timeout => Outcome::Timeout,
            SavedOutcome::SpawnFailed => Outcome::SpawnFailed,
            SavedOutcome::StartLimit => Outcome::StartLimit,
            SavedOutcome::Dependency => Outcome::Dependency,
        }
    }
}

/// An active timer. Times are milliseconds since 1970, UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTimer {
    pub activated: u64,
    #[serde(default)]
    pub last_trigger: Option<u64>,
    /// It elapsed, and waits for the unit it started to come to rest.
    #[serde(default)]
    pub running: bool,
    /// When the unit it starts last started and stopped, which a new
    /// manager cannot see for itself.
    #[serde(default)]
    pub unit_started: Option<u64>,
    #[serde(default)]
    pub unit_stopped: Option<u64>,
}

pub fn to_millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn from_millis(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedUnit {
    pub main: Option<SavedProcess>,
    /// Every process in the job when it was last saved, the main one included.
    pub processes: Vec<SavedProcess>,
    /// A unit whose output was to go to the channel but goes to its file for
    /// this run, and why: the `steward-cat` could not be started. The next
    /// manager's status says so too, rather than claiming the channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_fallback: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedProcess {
    pub pid: u32,
    /// Creation time (FILETIME): a reused PID is not the same process.
    pub created: u64,
}

pub fn path(state_dir: &Path, session: u32) -> PathBuf {
    state_dir.join(format!("state-{session}.json"))
}

/// The saved state, or nothing if there is none or it cannot be read.
pub fn load(path: &Path) -> Result<Saved, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Saved::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Record `saved`; with nothing running and nothing at rest there is nothing
/// to record, and the file goes.
pub fn save(path: &Path, saved: &Saved) -> io::Result<()> {
    if saved.is_empty() {
        return remove(path);
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(saved)?)?;
    std::fs::rename(&temporary, path)
}

/// Forget the state: the next manager starts afresh, as at sign-in.
pub fn remove(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn stamp_path(state_dir: &Path, timer: &str) -> PathBuf {
    state_dir.join("timers").join(timer)
}

/// When a persistent timer last elapsed, if it has left a stamp.
pub fn read_stamp(state_dir: &Path, timer: &str) -> Option<SystemTime> {
    let text = std::fs::read_to_string(stamp_path(state_dir, timer)).ok()?;
    text.trim().parse().ok().map(from_millis)
}

/// Record that a persistent timer elapsed at `t`: milliseconds since 1970,
/// UTC, as text.
pub fn write_stamp(state_dir: &Path, timer: &str, t: SystemTime) -> io::Result<()> {
    let path = stamp_path(state_dir, timer);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, format!("{}\n", to_millis(t)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_absence() {
        let dir = std::env::temp_dir().join(format!("steward-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = path(&dir, 3);
        assert!(file.ends_with("state-3.json"));
        assert_eq!(load(&file).unwrap(), Saved::default());
        let mut saved = Saved::default();
        saved.units.insert(
            "whkd.service".into(),
            SavedUnit {
                main: Some(SavedProcess {
                    pid: 42,
                    created: 7,
                }),
                processes: vec![
                    SavedProcess {
                        pid: 42,
                        created: 7,
                    },
                    SavedProcess {
                        pid: 43,
                        created: 9,
                    },
                ],
                output_fallback: None,
            },
        );
        save(&file, &saved).unwrap();
        assert_eq!(load(&file).unwrap(), saved);
        // A state from before fallbacks were recorded still loads, and one
        // that records a fallback keeps it.
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(!text.contains("output_fallback"), "{text}");
        saved.units.get_mut("whkd.service").unwrap().output_fallback =
            Some("steward-cat.exe does not exist".into());
        save(&file, &saved).unwrap();
        assert_eq!(load(&file).unwrap(), saved);
        // A state from before targets were saved still loads.
        std::fs::write(&file, r#"{"units":{}}"#).unwrap();
        assert_eq!(load(&file).unwrap(), Saved::default());
        let targets = Saved {
            targets: vec!["tiling.target".into()],
            ..Saved::default()
        };
        save(&file, &targets).unwrap();
        assert_eq!(load(&file).unwrap(), targets);
        let mut timers = Saved::default();
        timers.timers.insert(
            "backup.timer".into(),
            SavedTimer {
                activated: 1_000,
                last_trigger: Some(2_000),
                running: true,
                unit_started: Some(2_001),
                unit_stopped: None,
            },
        );
        save(&file, &timers).unwrap();
        assert_eq!(load(&file).unwrap(), timers);
        // Nothing running, but something at rest: that is worth a file.
        let mut rests = Saved {
            logon: Some(1_757_700_000_000),
            ..Saved::default()
        };
        rests.rests.insert(
            "late.timer".into(),
            SavedRest {
                state: RestState::Inactive,
                outcome: None,
                last_trigger: Some(3_000),
            },
        );
        rests.rests.insert(
            "crash.service".into(),
            SavedRest {
                state: RestState::Failed,
                outcome: Some(Outcome::Crashed(0xC000_0005).into()),
                last_trigger: None,
            },
        );
        save(&file, &rests).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains(r#""state": "failed""#), "{text}");
        assert!(text.contains(r#""crashed": 3221225477"#), "{text}");
        assert_eq!(load(&file).unwrap(), rests);
        // Nothing running and nothing at rest: no file, and none needed to
        // say so, whichever sign-in it is.
        let logon_only = Saved {
            logon: Some(1),
            ..Saved::default()
        };
        save(&file, &logon_only).unwrap();
        assert!(!file.exists());
        save(&file, &Saved::default()).unwrap();
        std::fs::write(&file, "not json").unwrap();
        assert!(load(&file).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn outcomes_survive_the_file() {
        use Outcome::*;
        for outcome in [
            Clean,
            ExitCode(3),
            Interrupted,
            Crashed(0xC000_0409),
            Vanished,
            Timeout,
            SpawnFailed,
            StartLimit,
            Dependency,
        ] {
            let text = serde_json::to_string(&SavedOutcome::from(outcome)).unwrap();
            let back: SavedOutcome = serde_json::from_str(&text).unwrap();
            assert_eq!(Outcome::from(back), outcome, "{text}");
        }
    }

    #[test]
    fn a_state_belongs_to_one_sign_in() {
        let mine = Saved {
            logon: Some(1_000),
            ..Saved::default()
        };
        assert!(mine.same_sign_in(Some(1_000)));
        // The session number was reused by a later sign-in.
        assert!(!mine.same_sign_in(Some(2_000)));
        // This manager cannot tell when its session began.
        assert!(!mine.same_sign_in(None));
        // A file from before sign-ins were recorded.
        assert!(Saved::default().same_sign_in(Some(2_000)));
    }

    #[test]
    fn stamps() {
        let dir = std::env::temp_dir().join(format!("steward-stamps-{}", std::process::id()));
        assert_eq!(read_stamp(&dir, "backup.timer"), None);
        let t = from_millis(1_757_700_000_123);
        write_stamp(&dir, "backup.timer", t).unwrap();
        assert_eq!(read_stamp(&dir, "backup.timer"), Some(t));
        assert_eq!(to_millis(t), 1_757_700_000_123);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
