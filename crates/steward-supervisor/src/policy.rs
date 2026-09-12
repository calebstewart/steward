//! How a service ended, whether that earns a restart, how long to wait first,
//! and when to stop trying.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use steward_unit::{Restart, Service};

/// Why a service (or one of its commands) ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Exit code 0, or ended by the Ctrl+C it was sent.
    Clean,
    /// A non-zero exit code.
    ExitCode(u32),
    /// An exception: the exit code is an NTSTATUS error (0xC0000000 and up),
    /// such as an access violation or a failed stack check.
    Crashed(u32),
    /// Every process of a `Type=forking` service went away on its own.
    Vanished,
    /// It did not start, or stop, in time.
    Timeout,
    /// A command could not be started at all (no such file, ...).
    SpawnFailed,
    /// Refused to start: too many starts within `StartLimitIntervalSec=`.
    StartLimit,
    /// A unit it `Requires=` did not start.
    Dependency,
}

impl Outcome {
    pub fn is_clean(self) -> bool {
        self == Outcome::Clean
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Clean => write!(f, "exited cleanly"),
            Outcome::ExitCode(code) => write!(f, "exited with code {code}"),
            Outcome::Crashed(code) => write!(f, "crashed (exception 0x{code:08X})"),
            Outcome::Vanished => write!(f, "all of its processes exited"),
            Outcome::Timeout => write!(f, "timed out"),
            Outcome::SpawnFailed => write!(f, "could not be started"),
            Outcome::StartLimit => write!(f, "was started too often (StartLimitBurst=)"),
            Outcome::Dependency => write!(f, "a unit it requires failed"),
        }
    }
}

/// The ending a process exit code describes.
pub fn classify(code: u32) -> Outcome {
    // What a console program's default handler exits with on Ctrl+C: the stop
    // steward asked for, not a crash.
    const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;
    match code {
        0 | STATUS_CONTROL_C_EXIT => Outcome::Clean,
        c if c >= 0xC000_0000 => Outcome::Crashed(c),
        c => Outcome::ExitCode(c),
    }
}

/// Whether `Restart=` asks for a restart after an unrequested ending.
pub fn should_restart(policy: Restart, outcome: Outcome) -> bool {
    use Outcome::*;
    match outcome {
        // Restarting cannot help these; the start limit is the verdict itself.
        StartLimit | Dependency => false,
        _ => match policy {
            Restart::No => false,
            Restart::Always => true,
            Restart::OnSuccess => outcome.is_clean(),
            Restart::OnFailure => !outcome.is_clean(),
            Restart::OnAbnormal => matches!(outcome, Crashed(_) | Timeout),
        },
    }
}

/// The wait before automatic restart number `step` (0 = the first):
/// `RestartSec=` growing to `RestartMaxDelaySec=` over `RestartSteps=`
/// restarts, exponentially, as systemd interpolates it.
pub fn restart_delay(service: &Service, step: u32) -> Duration {
    let base = service.restart_sec;
    let max = service.restart_max_delay;
    let steps = service.restart_steps;
    if steps == 0 || max <= base {
        return base;
    }
    let fraction = f64::from(step.min(steps)) / f64::from(steps);
    let seconds = if base.is_zero() {
        max.as_secs_f64() * fraction
    } else {
        let ratio = max.as_secs_f64() / base.as_secs_f64();
        base.as_secs_f64() * ratio.powf(fraction)
    };
    Duration::from_secs_f64(seconds).min(max)
}

/// `StartLimitBurst=` starts within `StartLimitIntervalSec=`; a burst or
/// interval of zero turns the limit off.
#[derive(Debug, Clone, Default)]
pub struct StartLimit {
    starts: VecDeque<Instant>,
}

impl StartLimit {
    /// Record a start at `now`, unless it would exceed the limit.
    pub fn allow(&mut self, service: &Service, now: Instant) -> bool {
        let (burst, interval) = (service.start_limit_burst, service.start_limit_interval);
        if burst == 0 || interval.is_zero() {
            return true;
        }
        while self
            .starts
            .front()
            .is_some_and(|&t| now.duration_since(t) >= interval)
        {
            self.starts.pop_front();
        }
        if self.starts.len() >= burst as usize {
            return false;
        }
        self.starts.push_back(now);
        true
    }

    /// Forget past starts (a start the user asked for after a failure).
    pub fn reset(&mut self) {
        self.starts.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use steward_unit::parse_service;

    fn service(extra: &str) -> Service {
        parse_service("t.service", &format!("[Service]\nExecStart=x\n{extra}"))
            .service
            .unwrap()
    }

    #[test]
    fn exit_codes() {
        assert_eq!(classify(0), Outcome::Clean);
        assert_eq!(classify(0xC000_013A), Outcome::Clean);
        assert_eq!(classify(1), Outcome::ExitCode(1));
        assert_eq!(classify(0xC000_0005), Outcome::Crashed(0xC000_0005));
        assert_eq!(classify(0xC000_0409), Outcome::Crashed(0xC000_0409));
    }

    #[test]
    fn restart_policies() {
        use Outcome::*;
        let cases = [
            (Restart::No, [false, false, false, false, false]),
            (Restart::Always, [true, true, true, true, true]),
            (Restart::OnSuccess, [true, false, false, false, false]),
            (Restart::OnFailure, [false, true, true, true, true]),
            (Restart::OnAbnormal, [false, false, true, true, false]),
        ];
        let outcomes = [
            Clean,
            ExitCode(1),
            Crashed(0xC000_0005),
            Timeout,
            SpawnFailed,
        ];
        for (policy, expected) in cases {
            for (outcome, want) in outcomes.iter().zip(expected) {
                assert_eq!(
                    should_restart(policy, *outcome),
                    want,
                    "{policy:?} {outcome:?}"
                );
            }
            assert!(!should_restart(policy, StartLimit));
        }
    }

    #[test]
    fn default_backoff_grows_from_a_second_to_a_minute() {
        let s = service("");
        let delays: Vec<f64> = (0..8)
            .map(|n| (restart_delay(&s, n).as_secs_f64() * 10.0).round() / 10.0)
            .collect();
        assert_eq!(delays, [1.0, 2.3, 5.1, 11.7, 26.5, 60.0, 60.0, 60.0]);
    }

    #[test]
    fn no_backoff() {
        let s = service("RestartSec=3s\nRestartSteps=0\n");
        assert_eq!(restart_delay(&s, 0), Duration::from_secs(3));
        assert_eq!(restart_delay(&s, 9), Duration::from_secs(3));
        let zero = service("RestartSec=0\nRestartSteps=2\nRestartMaxDelaySec=10s\n");
        assert_eq!(restart_delay(&zero, 0), Duration::ZERO);
        assert_eq!(restart_delay(&zero, 1), Duration::from_secs(5));
    }

    #[test]
    fn start_limit() {
        let s = service("StartLimitBurst=3\nStartLimitIntervalSec=10s\n");
        let t0 = Instant::now();
        let mut limit = StartLimit::default();
        let at = |secs| t0 + Duration::from_secs(secs);
        assert!(limit.allow(&s, at(0)));
        assert!(limit.allow(&s, at(1)));
        assert!(limit.allow(&s, at(2)));
        assert!(!limit.allow(&s, at(3)));
        // The first start has aged out.
        assert!(limit.allow(&s, at(10)));
        limit.reset();
        assert!(limit.allow(&s, at(11)));
        let unlimited = service("StartLimitBurst=0\n");
        let mut limit = StartLimit::default();
        assert!((0..100).all(|n| limit.allow(&unlimited, at(n / 10))));
    }
}
