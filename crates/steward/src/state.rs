//! What a restarted manager needs to take its services back: for each running
//! service, every process in its job and which of them is the main one;
//! which targets were active; and each active timer's schedule.
//! Written on every change to `%LOCALAPPDATA%\steward\state-<session>.json` (a
//! temporary file renamed over the old, so a crash mid-write leaves the
//! previous state rather than half of one), and removed when nothing is left
//! running. One per session, as managers are: a session's services are its
//! own manager's to adopt.
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub units: BTreeMap<String, SavedUnit>,
    /// The targets that were active. They have no processes to find again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    /// The timers that were active, and where each was in its schedule.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub timers: BTreeMap<String, SavedTimer>,
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

/// Record `saved`; with nothing running there is nothing to record, and the
/// file goes.
pub fn save(path: &Path, saved: &Saved) -> io::Result<()> {
    if saved.units.is_empty() && saved.targets.is_empty() && saved.timers.is_empty() {
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(saved)?)?;
    std::fs::rename(&temporary, path)
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
            },
        );
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
        // Nothing running: no file, and none needed to say so.
        save(&file, &Saved::default()).unwrap();
        assert!(!file.exists());
        save(&file, &Saved::default()).unwrap();
        std::fs::write(&file, "not json").unwrap();
        assert!(load(&file).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
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
