//! Generic poll scheduling: a policy decides when a job runs next, the runner
//! sleeps and runs it. Knows nothing about any particular dashboard or source.

pub mod backoff;
pub mod interval;
pub mod jitter;

use std::time::Duration;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::metrics::SourceMetrics;

pub use backoff::backoff_delay;
pub use interval::IntervalPolicy;
pub use jitter::{between, jittered};

/// How a job run went, as far as scheduling cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// Worth retrying.
    Failed,
    /// Never retry before the next regular run (e.g. credentials rejected).
    Rejected,
}

/// Decides when a job runs. Times are local to the poller's timezone.
pub trait PollPolicy: Send {
    /// When to run first after startup.
    fn first_run(&mut self, now: DateTime<Tz>) -> DateTime<Tz>;
    /// When to run next, given the time just after a run and its outcome.
    fn next_run(&mut self, now: DateTime<Tz>, outcome: Outcome) -> DateTime<Tz>;
    /// Called when a run is due, just before the job starts. Returning a time
    /// postpones the run to it (e.g. the timer woke late, into a quiet period).
    fn recheck(&mut self, _now: DateTime<Tz>) -> Option<DateTime<Tz>> {
        None
    }
}

fn until(now: DateTime<Tz>, target: DateTime<Tz>) -> Duration {
    (target - now).to_std().unwrap_or_default()
}

/// Runs `job` whenever `policy` says, until `shutdown` is cancelled.
pub async fn run_poller<P, F, Fut>(
    name: String,
    tz: Tz,
    mut policy: P,
    shutdown: CancellationToken,
    metrics: SourceMetrics,
    mut job: F,
) where
    P: PollPolicy,
    F: FnMut() -> Fut,
    Fut: Future<Output = Outcome>,
{
    let mut target = policy.first_run(now_in(tz));
    loop {
        info!(
            "{name}: next run at {}",
            target.format("%Y-%m-%d %H:%M:%S %Z")
        );
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(until(now_in(tz), target)) => {}
        }
        if let Some(later) = policy.recheck(now_in(tz)) {
            target = later;
            continue;
        }
        let started = tokio::time::Instant::now();
        let outcome = tokio::select! {
            biased;
            () = shutdown.cancelled() => return,
            o = job() => o,
        };
        metrics.observe(outcome, started.elapsed());
        target = policy.next_run(now_in(tz), outcome);
    }
}

fn now_in(tz: Tz) -> DateTime<Tz> {
    Utc::now().with_timezone(&tz)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Recorder;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use chrono::DateTime;
    use chrono_tz::Tz;
    use tokio_util::sync::CancellationToken;

    const MIN: Duration = Duration::from_secs(60);

    struct Scripted {
        first: chrono::Duration,
        next: chrono::Duration,
    }

    impl PollPolicy for Scripted {
        fn first_run(&mut self, now: DateTime<Tz>) -> DateTime<Tz> {
            now + self.first
        }
        fn next_run(&mut self, now: DateTime<Tz>, _: Outcome) -> DateTime<Tz> {
            now + self.next
        }
    }

    #[tokio::test(start_paused = true)]
    async fn runs_on_policy_schedule_until_shutdown() {
        let runs = Arc::new(AtomicU32::new(0));
        let shutdown = CancellationToken::new();
        let policy = Scripted {
            first: chrono::Duration::minutes(1),
            next: chrono::Duration::minutes(10),
        };
        let r = runs.clone();
        let task = tokio::spawn(run_poller(
            "test".into(),
            chrono_tz::UTC,
            policy,
            shutdown.clone(),
            Recorder::new(&[], &[]).source("t", "t"),
            move || {
                let r = r.clone();
                async move {
                    r.fetch_add(1, Ordering::SeqCst);
                    Outcome::Ok
                }
            },
        ));
        for _ in 0..25 {
            tokio::time::advance(MIN).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(runs.load(Ordering::SeqCst), 3);
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn past_target_runs_immediately() {
        let runs = Arc::new(AtomicU32::new(0));
        let shutdown = CancellationToken::new();
        let policy = Scripted {
            first: chrono::Duration::minutes(-5),
            next: chrono::Duration::minutes(-5),
        };
        let (r, s) = (runs.clone(), shutdown.clone());
        let task = tokio::spawn(run_poller(
            "test".into(),
            chrono_tz::UTC,
            policy,
            shutdown.clone(),
            Recorder::new(&[], &[]).source("t", "t"),
            move || {
                let (r, s) = (r.clone(), s.clone());
                async move {
                    if r.fetch_add(1, Ordering::SeqCst) + 1 >= 3 {
                        s.cancel();
                    }
                    Outcome::Ok
                }
            },
        ));
        task.await.unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 3);
    }

    /// Postpones the first due run once by 10 minutes, then runs normally.
    struct Postponing {
        postponed: bool,
    }

    impl PollPolicy for Postponing {
        fn first_run(&mut self, now: DateTime<Tz>) -> DateTime<Tz> {
            now + chrono::Duration::minutes(1)
        }
        fn next_run(&mut self, now: DateTime<Tz>, _: Outcome) -> DateTime<Tz> {
            now + chrono::Duration::hours(1)
        }
        fn recheck(&mut self, now: DateTime<Tz>) -> Option<DateTime<Tz>> {
            if self.postponed {
                return None;
            }
            self.postponed = true;
            Some(now + chrono::Duration::minutes(10))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn recheck_can_postpone_a_due_run() {
        let runs = Arc::new(AtomicU32::new(0));
        let shutdown = CancellationToken::new();
        let r = runs.clone();
        let task = tokio::spawn(run_poller(
            "test".into(),
            chrono_tz::UTC,
            Postponing { postponed: false },
            shutdown.clone(),
            Recorder::new(&[], &[]).source("t", "t"),
            move || {
                let r = r.clone();
                async move {
                    r.fetch_add(1, Ordering::SeqCst);
                    Outcome::Ok
                }
            },
        ));
        for _ in 0..5 {
            tokio::time::advance(MIN).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(runs.load(Ordering::SeqCst), 0, "postponed run ran early");
        for _ in 0..8 {
            tokio::time::advance(MIN).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn records_outcomes_and_durations() {
        let recorder = Recorder::new(&[], &[]);
        let shutdown = CancellationToken::new();
        let policy = Scripted {
            first: chrono::Duration::minutes(-5),
            next: chrono::Duration::minutes(-5),
        };
        let runs = Arc::new(AtomicU32::new(0));
        let (r, s) = (runs.clone(), shutdown.clone());
        let task = tokio::spawn(run_poller(
            "test".into(),
            chrono_tz::UTC,
            policy,
            shutdown.clone(),
            recorder.source("family", "calendar"),
            move || {
                let (r, s) = (r.clone(), s.clone());
                async move {
                    let n = r.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    match n {
                        0 => Outcome::Ok,
                        1 => Outcome::Failed,
                        _ => {
                            s.cancel();
                            Outcome::Rejected
                        }
                    }
                }
            },
        ));
        task.await.unwrap();
        let mut registry = prometheus_client::registry::Registry::default();
        recorder.register(&mut registry);
        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &registry).unwrap();
        for o in ["ok", "failed", "rejected"] {
            let line = format!(
                "tablica_source_fetches_total{{dashboard=\"family\",source=\"calendar\",outcome=\"{o}\"}} 1"
            );
            assert!(out.contains(&line), "missing {line} in\n{out}");
        }
        let base = "tablica_source_fetch_duration_seconds";
        let labels = "dashboard=\"family\",source=\"calendar\"";
        assert!(
            out.contains(&format!("{base}_count{{{labels}}} 3")),
            "{out}"
        );
        assert!(
            out.contains(&format!("{base}_sum{{{labels}}} 6.0")),
            "{out}"
        );
        assert!(
            out.contains(&format!("{base}_bucket{{le=\"2.5\",{labels}}} 3")),
            "{out}"
        );
    }
}
