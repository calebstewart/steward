//! A timer's schedule: when it next elapses. It is worked out afresh each
//! time from what has happened -- when the timer was started, when it last
//! triggered, when the unit it starts last started and stopped -- so a
//! changed definition, a changed time zone, or a clock set right takes
//! effect by itself.
//!
//! The rules are systemd's (timer.c):
//! - A calendar trigger counts from the last trigger, or from the timer's
//!   start if it has not triggered. An elapse missed while the machine slept
//!   is due at once, and once, however many were missed.
//! - `OnActiveSec=`, `OnBootSec=` and `OnStartupSec=` elapse once. One that
//!   has already passed when the timer starts is due at once, unless the
//!   timer has triggered before; then it is spent.
//! - `OnUnitActiveSec=` and `OnUnitInactiveSec=` count from the later of the
//!   unit's last start (or stop) and the last trigger, and from nothing --
//!   they do not elapse -- until one of those has happened.
//! - Having triggered, the timer waits until the unit it started is at rest
//!   before it elapses again, so it never starts a unit that is still
//!   running from its last trigger.
//!
//! Every time is the wall clock's, the relative ones included: time asleep
//! counts, where systemd's monotonic clock stops for it. A timer meant to run
//! every hour runs an hour after it last did, however long of that the
//! machine slept -- which is what a Scheduled Task's repetition does too.

use std::time::{Duration, SystemTime};

use steward_unit::calendar::Zone;
use steward_unit::{Timer, Trigger};

/// What the relative triggers count from.
#[derive(Debug, Clone, Copy)]
pub struct Moments {
    /// When Windows started.
    pub boot: SystemTime,
    /// Sign-in: when the session started, and steward with it.
    pub startup: SystemTime,
    /// When the unit the timer starts last started, if it has.
    pub unit_started: Option<SystemTime>,
    /// When it last came to rest, if it has.
    pub unit_stopped: Option<SystemTime>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Schedule {
    /// When the timer was started; `None` while it is not running.
    pub activated: Option<SystemTime>,
    /// When it last elapsed.
    pub last_trigger: Option<SystemTime>,
    /// It elapsed, and waits for the unit it started to be at rest again.
    pub running: bool,
    /// This round's share of `RandomizedDelaySec=`.
    delay: Duration,
}

impl Schedule {
    /// The timer was started. A persistent timer's last trigger is `stamp`,
    /// what it recorded when it last elapsed; any other's is forgotten.
    /// `random` is any random number, for `RandomizedDelaySec=`.
    pub fn start(
        &mut self,
        timer: &Timer,
        name: &str,
        now: SystemTime,
        stamp: Option<SystemTime>,
        random: u64,
    ) {
        self.activated = Some(now);
        // A stamp from the future is a clock that was wrong, or is now.
        self.last_trigger = stamp.filter(|&t| timer.persistent && t <= now);
        self.running = false;
        self.roll(timer, name, random);
    }

    /// Take over a timer a previous manager had started, as it was.
    pub fn adopt(
        &mut self,
        timer: &Timer,
        name: &str,
        activated: SystemTime,
        last_trigger: Option<SystemTime>,
        running: bool,
        random: u64,
    ) {
        self.activated = Some(activated);
        self.last_trigger = last_trigger;
        self.running = running;
        self.roll(timer, name, random);
    }

    pub fn stop(&mut self) {
        self.activated = None;
        self.running = false;
    }

    /// It elapsed at `now`, and its unit is being started.
    pub fn fire(&mut self, timer: &Timer, name: &str, now: SystemTime, random: u64) {
        self.last_trigger = Some(now);
        self.running = true;
        self.roll(timer, name, random);
    }

    /// The unit it started is at rest: it may elapse again.
    pub fn unit_at_rest(&mut self) {
        self.running = false;
    }

    /// When it next elapses: the earliest of its triggers, delayed by its
    /// share of `RandomizedDelaySec=`. `None` while it is stopped, while it
    /// waits for its unit, or when nothing more is due.
    pub fn next(
        &self,
        timer: &Timer,
        moments: &Moments,
        now: SystemTime,
        zone: &dyn Zone,
    ) -> Option<SystemTime> {
        let activated = self.activated?;
        if self.running {
            return None;
        }
        let after = |base: Option<SystemTime>, span: &Duration| base?.checked_add(*span);
        timer
            .triggers
            .iter()
            .filter_map(|trigger| {
                let elapse = match trigger {
                    Trigger::Active(span) => after(Some(activated), span),
                    Trigger::Boot(span) => after(Some(moments.boot), span),
                    Trigger::Startup(span) => after(Some(moments.startup), span),
                    Trigger::UnitActive(span) => {
                        after(later(moments.unit_started, self.last_trigger), span)
                    }
                    Trigger::UnitInactive(span) => {
                        after(later(moments.unit_stopped, self.last_trigger), span)
                    }
                    Trigger::Calendar(calendar) => {
                        calendar.next_after(self.last_trigger.unwrap_or(activated), zone)
                    }
                }?;
                let spent = trigger.is_once() && self.last_trigger.is_some() && elapse < now;
                (!spent).then_some(elapse)
            })
            .min()?
            .checked_add(self.delay)
    }

    /// Nothing more will ever be due, and the timer is to become inactive
    /// rather than stay elapsed (`RemainAfterElapse=no`). A timer that
    /// follows its unit is never done: the unit may run again.
    pub fn is_done(&self, timer: &Timer, next: Option<SystemTime>) -> bool {
        !timer.remain_after_elapse
            && !self.running
            && next.is_none()
            && !timer.triggers.iter().any(Trigger::follows_unit)
    }

    /// A delay from 0 up to (not including) `RandomizedDelaySec=`.
    fn roll(&mut self, timer: &Timer, name: &str, random: u64) {
        let bits = if timer.fixed_random_delay {
            fnv1a(name)
        } else {
            random
        };
        // The spread, to 584 years, times a fraction of 53 random bits.
        let spread = timer.randomized_delay.as_nanos().min(u128::from(u64::MAX));
        let nanos = (spread * u128::from(bits >> 11)) >> 53;
        self.delay = Duration::from_nanos(nanos as u64);
    }
}

fn later(a: Option<SystemTime>, b: Option<SystemTime>) -> Option<SystemTime> {
    a.max(b)
}

/// The same number for the same name, every time.
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use steward_unit::calendar::{Civil, Utc};
    use steward_unit::parse_service;

    fn timer(section: &str) -> Timer {
        let parsed = parse_service("t.timer", &format!("[Timer]\n{section}"));
        assert!(!parsed.has_errors(), "{:?}", parsed.diagnostics);
        parsed.service.unwrap().timer.unwrap()
    }

    fn at(text: &str) -> SystemTime {
        let n: Vec<u32> = text
            .split(['-', ' ', ':'])
            .map(|n| n.parse().unwrap())
            .collect();
        Utc.moment(Civil {
            year: n[0] as i32,
            month: n[1],
            day: n[2],
            hour: n[3],
            minute: n[4],
            second: n[5],
        })
        .unwrap()
    }

    fn show(t: Option<SystemTime>) -> Option<String> {
        t.map(|t| Utc.civil(t).to_string())
    }

    const SECOND: Duration = Duration::from_secs(1);

    /// A timer, the clock, and what its unit did.
    struct Harness {
        timer: Timer,
        schedule: Schedule,
        moments: Moments,
        now: SystemTime,
    }

    impl Harness {
        fn new(section: &str, now: &str) -> Harness {
            let now = at(now);
            Harness {
                timer: timer(section),
                schedule: Schedule::default(),
                moments: Moments {
                    boot: now - Duration::from_secs(3600),
                    startup: now - Duration::from_secs(60),
                    unit_started: None,
                    unit_stopped: None,
                },
                now,
            }
        }

        fn start(&mut self) {
            self.schedule
                .start(&self.timer, "t.timer", self.now, None, 0);
        }

        fn next(&self) -> Option<String> {
            show(self.next_time())
        }

        fn next_time(&self) -> Option<SystemTime> {
            self.schedule
                .next(&self.timer, &self.moments, self.now, &Utc)
        }

        fn wait_until(&mut self, text: &str) {
            self.now = at(text);
        }

        /// Elapse now, if due, and run the unit for `runs`.
        fn elapse(&mut self, runs: Duration) -> bool {
            if !self.next_time().is_some_and(|t| t <= self.now) {
                return false;
            }
            self.schedule.fire(&self.timer, "t.timer", self.now, 0);
            assert_eq!(self.next(), None, "waits for its unit");
            self.moments.unit_started = Some(self.now);
            self.now += runs;
            self.moments.unit_stopped = Some(self.now);
            self.schedule.unit_at_rest();
            true
        }
    }

    #[test]
    fn a_calendar_timer_counts_from_its_start_then_from_its_last_trigger() {
        let mut h = Harness::new("OnCalendar=*-*-* 10:00\n", "2026-09-12 09:00:00");
        assert_eq!(h.next(), None, "not started");
        h.start();
        assert_eq!(h.next().as_deref(), Some("2026-09-12 10:00:00"));
        h.wait_until("2026-09-12 10:00:00");
        assert!(h.elapse(SECOND * 5));
        assert_eq!(h.next().as_deref(), Some("2026-09-13 10:00:00"));
        h.schedule.stop();
        assert_eq!(h.next(), None);
    }

    #[test]
    fn a_timer_started_after_today_s_elapse_waits_for_tomorrow_s() {
        let mut h = Harness::new("OnCalendar=*-*-* 10:00\n", "2026-09-12 11:00:00");
        h.start();
        assert_eq!(h.next().as_deref(), Some("2026-09-13 10:00:00"));
    }

    #[test]
    fn elapses_missed_asleep_are_made_up_once() {
        let mut h = Harness::new("OnCalendar=hourly\n", "2026-09-12 09:30:00");
        h.start();
        // Asleep from before 10:00 until 13:20.
        h.wait_until("2026-09-12 13:20:00");
        assert_eq!(h.next().as_deref(), Some("2026-09-12 10:00:00"));
        assert!(h.elapse(SECOND));
        assert_eq!(h.next().as_deref(), Some("2026-09-12 14:00:00"));
    }

    #[test]
    fn a_persistent_timer_makes_up_what_it_missed_while_stopped() {
        let mut h = Harness::new(
            "OnCalendar=*-*-* 10:00\nPersistent=true\n",
            "2026-09-12 12:00:00",
        );
        let yesterday = Some(at("2026-09-11 10:00:02"));
        h.schedule.start(&h.timer, "t.timer", h.now, yesterday, 0);
        assert_eq!(h.next().as_deref(), Some("2026-09-12 10:00:00"));
        assert!(h.elapse(SECOND));
        assert_eq!(h.next().as_deref(), Some("2026-09-13 10:00:00"));
        // A stamp from the future is ignored.
        let tomorrow = Some(at("2026-09-13 10:00:00"));
        h.schedule.start(&h.timer, "t.timer", h.now, tomorrow, 0);
        assert_eq!(h.schedule.last_trigger, None);
        // Without Persistent=, the stamp means nothing.
        let mut h = Harness::new("OnCalendar=*-*-* 10:00\n", "2026-09-12 12:00:00");
        h.schedule.start(&h.timer, "t.timer", h.now, yesterday, 0);
        assert_eq!(h.next().as_deref(), Some("2026-09-13 10:00:00"));
    }

    #[test]
    fn one_time_triggers_elapse_once() {
        let mut h = Harness::new("OnActiveSec=10s\n", "2026-09-12 09:00:00");
        h.start();
        assert_eq!(h.next().as_deref(), Some("2026-09-12 09:00:10"));
        h.wait_until("2026-09-12 09:00:10");
        assert!(h.elapse(SECOND));
        assert_eq!(h.next(), None);
        // Started again, it counts from its new start.
        h.start();
        assert_eq!(h.next().as_deref(), Some("2026-09-12 09:00:21"));
    }

    #[test]
    fn a_one_time_trigger_already_past_is_due_at_once() {
        // Booted an hour ago; signed in a minute ago.
        let mut h = Harness::new("OnBootSec=5min\nOnStartupSec=10s\n", "2026-09-12 09:00:00");
        h.start();
        assert_eq!(h.next().as_deref(), Some("2026-09-12 08:05:00"));
        assert!(h.elapse(SECOND));
        // Both have passed, and it has triggered: spent.
        assert_eq!(h.next(), None);
    }

    #[test]
    fn after_boot_then_every_hour_the_unit_runs() {
        let mut h = Harness::new(
            "OnBootSec=90min\nOnUnitActiveSec=1h\n",
            "2026-09-12 09:00:00",
        );
        h.start();
        // The unit has never run: only the boot trigger counts.
        assert_eq!(h.next().as_deref(), Some("2026-09-12 09:30:00"));
        h.wait_until("2026-09-12 09:30:00");
        assert!(h.elapse(SECOND * 30));
        assert_eq!(h.next().as_deref(), Some("2026-09-12 10:30:00"));
        h.wait_until("2026-09-12 10:30:00");
        assert!(h.elapse(SECOND * 30));
        assert_eq!(h.next().as_deref(), Some("2026-09-12 11:30:00"));
    }

    #[test]
    fn on_unit_inactive_counts_from_the_end_of_the_last_run() {
        let mut h = Harness::new(
            "OnActiveSec=0\nOnUnitInactiveSec=10min\n",
            "2026-09-12 09:00:00",
        );
        h.start();
        assert!(h.elapse(Duration::from_secs(120)));
        assert_eq!(h.next().as_deref(), Some("2026-09-12 09:12:00"));
    }

    #[test]
    fn a_unit_run_by_hand_counts_too() {
        let mut h = Harness::new("OnUnitActiveSec=1h\n", "2026-09-12 09:00:00");
        h.start();
        assert_eq!(h.next(), None, "the unit has not run");
        h.moments.unit_started = Some(at("2026-09-12 09:15:00"));
        assert_eq!(h.next().as_deref(), Some("2026-09-12 10:15:00"));
    }

    #[test]
    fn remain_after_elapse() {
        let mut h = Harness::new("OnActiveSec=0\n", "2026-09-12 09:00:00");
        h.start();
        assert!(!h.schedule.is_done(&h.timer, h.next_time()));
        h.elapse(SECOND);
        // Elapsed, and stays active by default.
        assert!(!h.schedule.is_done(&h.timer, h.next_time()));
        h.timer.remain_after_elapse = false;
        assert!(h.schedule.is_done(&h.timer, h.next_time()));
        // Not while its unit may still run and start the count again.
        let h = Harness::new(
            "OnUnitActiveSec=1h\nRemainAfterElapse=no\n",
            "2026-09-12 09:00:00",
        );
        assert!(!h.schedule.is_done(&h.timer, None));
    }

    #[test]
    fn randomized_delay() {
        let mut h = Harness::new(
            "OnCalendar=*-*-* 10:00\nRandomizedDelaySec=1h\n",
            "2026-09-12 09:00:00",
        );
        let base = at("2026-09-12 10:00:00");
        let mut delays = Vec::new();
        for random in [0, 1 << 63, u64::MAX] {
            h.schedule.start(&h.timer, "t.timer", h.now, None, random);
            let delay = h.next_time().unwrap().duration_since(base).unwrap();
            assert!(delay < Duration::from_secs(3600), "{delay:?}");
            delays.push(delay.as_secs());
        }
        assert_eq!(delays, [0, 1800, 3599]);
        // A fixed delay is the same, whatever the dice say.
        h.timer.fixed_random_delay = true;
        let fixed: Vec<_> = [1, 2]
            .into_iter()
            .map(|random| {
                h.schedule.start(&h.timer, "t.timer", h.now, None, random);
                h.next_time()
            })
            .collect();
        assert_eq!(fixed[0], fixed[1]);
        assert_ne!(fixed[0], Some(base));
    }

    #[test]
    fn adopted_as_it_was() {
        let mut h = Harness::new(
            "OnActiveSec=1h\nOnUnitActiveSec=1h\n",
            "2026-09-12 12:00:00",
        );
        h.schedule.adopt(
            &h.timer,
            "t.timer",
            at("2026-09-12 09:00:00"),
            Some(at("2026-09-12 10:00:00")),
            false,
            0,
        );
        // The one-time trigger is spent; the unit one counts from 10:00.
        assert_eq!(h.next().as_deref(), Some("2026-09-12 11:00:00"));
        h.schedule.adopt(
            &h.timer,
            "t.timer",
            at("2026-09-12 09:00:00"),
            None,
            true,
            0,
        );
        assert_eq!(h.next(), None, "still waiting for its unit");
    }
}
