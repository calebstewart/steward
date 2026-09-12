//! What a `.timer` unit says: when it elapses, and what it starts then.

use std::time::Duration;

use crate::calendar::Calendar;

/// One thing a timer waits for. Each trigger's elapse is worked out on its
/// own; the timer elapses at the earliest of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// `OnActiveSec=`: after the timer was started.
    Active(Duration),
    /// `OnBootSec=`: after Windows started.
    Boot(Duration),
    /// `OnStartupSec=`: after sign-in, when steward first started in the
    /// session.
    Startup(Duration),
    /// `OnUnitActiveSec=`: after the unit it starts last started.
    UnitActive(Duration),
    /// `OnUnitInactiveSec=`: after the unit it starts last stopped.
    UnitInactive(Duration),
    /// `OnCalendar=`: at the times a calendar event names.
    Calendar(Box<Calendar>),
}

impl Trigger {
    /// Elapses once, and not again once the timer has triggered: the
    /// triggers counted from the timer's start, boot, or sign-in.
    pub fn is_once(&self) -> bool {
        matches!(
            self,
            Trigger::Active(_) | Trigger::Boot(_) | Trigger::Startup(_)
        )
    }

    /// Counted from what the unit it starts does.
    pub fn follows_unit(&self) -> bool {
        matches!(self, Trigger::UnitActive(_) | Trigger::UnitInactive(_))
    }
}

/// A timer's `[Timer]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timer {
    pub triggers: Vec<Trigger>,
    /// `Unit=`: what it starts; by default the service named as it is.
    pub unit: String,
    /// `Persistent=`: the last trigger is kept on disk, so that a calendar
    /// elapse missed while the timer was not running (signed out) is made up
    /// at its next start.
    pub persistent: bool,
    /// `AccuracySec=`: how late systemd may be, to save wake-ups. steward
    /// looks every second, and so is never later than that.
    pub accuracy: Duration,
    /// `RandomizedDelaySec=`: each elapse is put off by up to this long.
    pub randomized_delay: Duration,
    /// `FixedRandomDelay=`: the same delay every time, not a new one.
    pub fixed_random_delay: bool,
    /// `RemainAfterElapse=`: a timer with nothing more to wait for stays
    /// active (elapsed) rather than becoming inactive.
    pub remain_after_elapse: bool,
}

impl Timer {
    pub(crate) fn new(name: &str) -> Timer {
        let stem = name.strip_suffix(".timer").unwrap_or(name);
        Timer {
            triggers: Vec::new(),
            unit: format!("{stem}.service"),
            persistent: false,
            accuracy: Duration::from_secs(60),
            randomized_delay: Duration::ZERO,
            fixed_random_delay: false,
            remain_after_elapse: true,
        }
    }
}
