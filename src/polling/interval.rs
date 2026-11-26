//! Fixed period with jitter.

use std::time::Duration;

use chrono::DateTime;
use chrono_tz::Tz;
use rand::rngs::StdRng;

use super::backoff::backoff_delay;
use super::jitter::{between, jittered};
use super::{Outcome, PollPolicy};

const FIRST_RUN_WITHIN: Duration = Duration::from_secs(2 * 60);

/// Runs every `period` (jittered by `spread`); after failures, backs off from
/// 5 min but never waits longer than the period. Rejections are not retried early.
pub struct IntervalPolicy {
    period: Duration,
    spread: f64,
    failures: u32,
    rng: StdRng,
}

impl IntervalPolicy {
    pub fn new(period: Duration, spread: f64, rng: StdRng) -> Self {
        Self {
            period,
            spread,
            failures: 0,
            rng,
        }
    }
}

impl PollPolicy for IntervalPolicy {
    fn first_run(&mut self, now: DateTime<Tz>) -> DateTime<Tz> {
        now + between(Duration::ZERO, FIRST_RUN_WITHIN, &mut self.rng)
    }

    fn next_run(&mut self, now: DateTime<Tz>, outcome: Outcome) -> DateTime<Tz> {
        let base = match outcome {
            Outcome::Ok | Outcome::Rejected => {
                self.failures = 0;
                self.period
            }
            Outcome::Failed => {
                let delay = backoff_delay(self.failures).min(self.period);
                self.failures = self.failures.saturating_add(1);
                delay
            }
        };
        now + jittered(base, self.spread, &mut self.rng)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::polling::{Outcome, PollPolicy};
    use chrono::TimeZone;
    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use std::time::Duration;

    const MIN: Duration = Duration::from_secs(60);

    fn now() -> chrono::DateTime<chrono_tz::Tz> {
        chrono_tz::Europe::Warsaw
            .with_ymd_and_hms(2026, 11, 25, 12, 0, 0)
            .unwrap()
    }

    fn gap(a: chrono::DateTime<chrono_tz::Tz>, b: chrono::DateTime<chrono_tz::Tz>) -> Duration {
        (b - a).to_std().unwrap()
    }

    #[test]
    fn first_run_within_two_minutes() {
        for seed in 0..50 {
            let mut p = IntervalPolicy::new(15 * MIN, 0.2, StdRng::seed_from_u64(seed));
            let d = gap(now(), p.first_run(now()));
            assert!(d < 2 * MIN, "{d:?}");
        }
    }

    #[test]
    fn ok_waits_period_jittered() {
        let mut p = IntervalPolicy::new(15 * MIN, 0.2, StdRng::seed_from_u64(3));
        for _ in 0..200 {
            let d = gap(now(), p.next_run(now(), Outcome::Ok));
            assert!(d >= 12 * MIN && d <= 18 * MIN, "{d:?}");
        }
    }

    #[test]
    fn failures_back_off_capped_at_period() {
        let mut p = IntervalPolicy::new(30 * MIN, 0.2, StdRng::seed_from_u64(4));
        let mut fail = || gap(now(), p.next_run(now(), Outcome::Failed));
        let d = fail();
        assert!(d >= 4 * MIN && d <= 6 * MIN, "{d:?}");
        let d = fail();
        assert!(d >= 8 * MIN && d <= 12 * MIN, "{d:?}");
        let d = fail();
        assert!(d >= 16 * MIN && d <= 24 * MIN, "{d:?}");
        let d = fail();
        assert!(d >= 24 * MIN && d <= 36 * MIN, "{d:?}");
        let d = gap(now(), p.next_run(now(), Outcome::Ok));
        assert!(d >= 24 * MIN, "{d:?}");
        let d = gap(now(), p.next_run(now(), Outcome::Failed));
        assert!(d >= 4 * MIN && d <= 6 * MIN, "{d:?}");
    }

    #[test]
    fn rejected_keeps_regular_cadence_and_clears_failures() {
        let mut p = IntervalPolicy::new(30 * MIN, 0.2, StdRng::seed_from_u64(5));
        p.next_run(now(), Outcome::Failed);
        p.next_run(now(), Outcome::Failed);
        let d = gap(now(), p.next_run(now(), Outcome::Rejected));
        assert!(d >= 24 * MIN && d <= 36 * MIN, "{d:?}");
        let d = gap(now(), p.next_run(now(), Outcome::Failed));
        assert!(d >= 4 * MIN && d <= 6 * MIN, "{d:?}");
    }
}
