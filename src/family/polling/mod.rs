//! How the family dashboard's sources are polled.

pub mod timetable_policy;
pub mod windows;

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rand::rngs::StdRng;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::family::FamilyCaches;
use crate::polling::{IntervalPolicy, Outcome, run_poller};
use crate::scheduler::{
    Sources, refresh_calendar_once, refresh_timetable_once, refresh_weather_once,
};
use crate::sources::SourceError;

pub use timetable_policy::{PlannedRuns, TimetablePolicy};

const CALENDAR_EVERY: Duration = Duration::from_secs(15 * 60);
const WEATHER_EVERY: Duration = Duration::from_secs(30 * 60);
const INTERVAL_SPREAD: f64 = 0.2;

/// How a fetch went, as far as scheduling cares.
fn to_outcome(r: &Result<(), SourceError>) -> Outcome {
    match r {
        Ok(()) => Outcome::Ok,
        Err(SourceError::CredentialsRejected(_)) => Outcome::Rejected,
        Err(_) => Outcome::Failed,
    }
}

fn fresh_rng() -> StdRng {
    rand::make_rng()
}

/// Starts the family dashboard's pollers: calendar, weather and one Librus
/// poller per kid. They run until `shutdown` is cancelled.
pub fn spawn_family_pollers(
    cfg: Arc<Config>,
    caches: Arc<FamilyCaches>,
    sources: Sources,
    shutdown: CancellationToken,
) -> Vec<JoinHandle<()>> {
    let tz = cfg.timezone;
    let mut handles = Vec::new();

    {
        let (cfg, caches, http) = (cfg.clone(), caches.clone(), sources.http.clone());
        let policy = IntervalPolicy::new(CALENDAR_EVERY, INTERVAL_SPREAD, fresh_rng());
        handles.push(tokio::spawn(run_poller(
            "calendar".into(),
            tz,
            policy,
            shutdown.clone(),
            move || {
                let (cfg, caches, http) = (cfg.clone(), caches.clone(), http.clone());
                async move {
                    let r = refresh_calendar_once(
                        &http,
                        &cfg.family.ics_url,
                        &caches.calendar,
                        Utc::now(),
                    )
                    .await;
                    to_outcome(&r)
                }
            },
        )));
    }

    {
        let (cfg, caches, src) = (cfg.clone(), caches.clone(), sources.clone());
        let policy = IntervalPolicy::new(WEATHER_EVERY, INTERVAL_SPREAD, fresh_rng());
        handles.push(tokio::spawn(run_poller(
            "weather".into(),
            tz,
            policy,
            shutdown.clone(),
            move || {
                let (cfg, caches, src) = (cfg.clone(), caches.clone(), src.clone());
                async move {
                    let r = refresh_weather_once(
                        &src.http,
                        &src.weather_base,
                        &cfg.family.weather,
                        cfg.timezone,
                        &caches.weather,
                        Utc::now(),
                    )
                    .await;
                    to_outcome(&r)
                }
            },
        )));
    }

    let planned = PlannedRuns::new(cfg.family.kids.len());
    for (index, kid) in cfg.family.kids.iter().enumerate() {
        let policy = TimetablePolicy::new(index, caches.clone(), planned.clone(), fresh_rng());
        let (kid, caches, urls) = (kid.clone(), caches.clone(), sources.librus.clone());
        handles.push(tokio::spawn(run_poller(
            format!("timetable ({})", kid.label),
            tz,
            policy,
            shutdown.clone(),
            move || {
                let (kid, caches, urls) = (kid.clone(), caches.clone(), urls.clone());
                async move {
                    let cache = &caches.timetables[index];
                    let r = refresh_timetable_once(&urls, &kid, cache, Utc::now()).await;
                    to_outcome(&r)
                }
            },
        )));
    }
    handles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_outcome_maps_rejected_credentials() {
        assert_eq!(to_outcome(&Ok(())), Outcome::Ok);
        assert_eq!(
            to_outcome(&Err(SourceError::CredentialsRejected("x".into()))),
            Outcome::Rejected
        );
        assert_eq!(to_outcome(&Err(SourceError::Status(500))), Outcome::Failed);
    }
}
