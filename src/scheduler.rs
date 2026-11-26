//! Background fetch loops, one per source.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Days, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::cache::{Caches, SourceCache};
use crate::config::{Config, KidConfig};
use crate::polling::backoff_delay;
use crate::sources::SourceError;
use crate::sources::calendar::{self, FetchOutcome, IcsSnapshot};
use crate::sources::librus::timetable::{self, Timetable};
use crate::sources::librus::{LibrusClient, LibrusUrls};
use crate::sources::weather::{self, Weather};

const CALENDAR_EVERY: Duration = Duration::from_secs(15 * 60);
const WEATHER_EVERY: Duration = Duration::from_secs(30 * 60);
/// Local hours of the Librus slots, each at :30.
const SLOT_HOURS: [u32; 9] = [5, 7, 9, 11, 13, 15, 17, 19, 21];

/// External endpoints; tests point them at mock servers.
#[derive(Clone)]
pub struct Sources {
    pub http: reqwest::Client,
    pub weather_base: String,
    pub librus: LibrusUrls,
}

/// The first Librus slot strictly after `now`: 05:30, 07:30, ..., 21:30 local,
/// then tomorrow's 05:30.
pub fn next_librus_slot(now: DateTime<Tz>) -> DateTime<Tz> {
    let tz = now.timezone();
    let mut date = now.date_naive();
    loop {
        if let Some(slot) = SLOT_HOURS
            .iter()
            .filter_map(|&h| slot_at(tz, date, h))
            .find(|slot| *slot > now)
        {
            return slot;
        }
        date = date.checked_add_days(Days::new(1)).unwrap_or(date);
    }
}

fn slot_at(tz: Tz, date: NaiveDate, hour: u32) -> Option<DateTime<Tz>> {
    tz.from_local_datetime(&date.and_hms_opt(hour, 30, 0)?)
        .earliest()
}

/// An error message that is safe to log: reqwest errors embed the request URL,
/// which for the calendar is a secret.
fn describe(err: &SourceError) -> String {
    match err {
        SourceError::Http(e) => {
            let msg = e.to_string();
            match e.url() {
                Some(url) => msg.replace(url.as_str(), "<url>"),
                None => msg,
            }
        }
        other => other.to_string(),
    }
}

fn fail<T: serde::Serialize + serde::de::DeserializeOwned + Clone>(
    source: &str,
    cache: &SourceCache<T>,
    err: SourceError,
    now: DateTime<Utc>,
) -> SourceError {
    let msg = describe(&err);
    warn!("{source} refresh failed: {msg}");
    cache.record_error(msg, now);
    match err {
        SourceError::Http(e) => SourceError::Http(e.without_url()),
        other => other,
    }
}

fn stored<T>(source: &str, result: std::io::Result<T>) {
    if let Err(e) = result {
        warn!("{source}: cannot persist cache: {e}");
    }
}

/// Fetches the calendar feed once. On 304 the data stays and only the
/// timestamp moves. Failures are logged and recorded; the old data is kept.
pub async fn refresh_calendar_once(
    http: &reqwest::Client,
    url: &str,
    cache: &SourceCache<IcsSnapshot>,
    now: DateTime<Utc>,
) -> Result<(), SourceError> {
    let current = cache.get();
    let etag = current.as_ref().and_then(|c| c.data.etag.clone());
    match calendar::fetch(http, url, etag.as_deref()).await {
        Ok(FetchOutcome::Fresh(snapshot)) => {
            if let Err(e) = calendar::validate(&snapshot.text) {
                return Err(fail("calendar", cache, e, now));
            }
            stored("calendar", cache.put(snapshot, now));
            info!("calendar refreshed");
            Ok(())
        }
        Ok(FetchOutcome::NotModified) => match current {
            Some(c) => {
                stored("calendar", cache.put(c.data, now));
                info!("calendar not modified");
                Ok(())
            }
            None => Err(fail(
                "calendar",
                cache,
                SourceError::Parse("304 without cached data".into()),
                now,
            )),
        },
        Err(e) => Err(fail("calendar", cache, e, now)),
    }
}

/// Fetches the forecast once.
pub async fn refresh_weather_once(
    http: &reqwest::Client,
    base_url: &str,
    cfg: &crate::config::WeatherConfig,
    tz: Tz,
    cache: &SourceCache<Weather>,
    now: DateTime<Utc>,
) -> Result<(), SourceError> {
    match weather::fetch(http, base_url, cfg, tz).await {
        Ok(w) => {
            stored("weather", cache.put(w, now));
            info!("weather refreshed");
            Ok(())
        }
        Err(e) => Err(fail("weather", cache, e, now)),
    }
}

/// Logs in as one kid (fresh cookie jar) and fetches two timetable weeks.
pub async fn refresh_timetable_once(
    urls: &LibrusUrls,
    kid: &KidConfig,
    cache: &SourceCache<Timetable>,
    now: DateTime<Utc>,
) -> Result<(), SourceError> {
    let client = LibrusClient::new(urls.clone());
    match timetable::fetch_two_weeks(&client, &kid.librus_login, &kid.librus_password).await {
        Ok(t) => {
            stored("timetable", cache.put(t, now));
            info!("timetable refreshed");
            Ok(())
        }
        Err(e) => Err(fail("timetable", cache, e, now)),
    }
}

/// Runs `job` now and then every `period` until shutdown.
async fn every<F, Fut>(period: Duration, shutdown: CancellationToken, job: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = job() => {}
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(period) => {}
        }
    }
}

/// Retries allowed after a failed Librus run, per slot.
const MAX_RETRIES_PER_SLOT: u32 = 2;

/// How long the Librus loop waits after a run, and the retry count to carry
/// into the next run. At most two retries per slot (5 then 10 min), never past
/// the next slot; the count resets whenever the next run is a slot. Rejected
/// credentials are never retried: the loop waits for the next slot.
fn librus_next_wait(
    now: DateTime<Tz>,
    retries_this_slot: u32,
    outcome: &Result<(), SourceError>,
) -> (Duration, u32) {
    let until_slot = (next_librus_slot(now) - now).to_std().unwrap_or_default();
    let rejected = matches!(outcome, Err(SourceError::CredentialsRejected(_)));
    if outcome.is_ok() || rejected || retries_this_slot >= MAX_RETRIES_PER_SLOT {
        return (until_slot, 0);
    }
    let delay = backoff_delay(retries_this_slot);
    if delay >= until_slot {
        (until_slot, 0)
    } else {
        (delay, retries_this_slot + 1)
    }
}

async fn librus_loop(
    tz: Tz,
    kid: KidConfig,
    caches: Arc<Caches>,
    index: usize,
    urls: LibrusUrls,
    shutdown: CancellationToken,
) {
    let cache = &caches.timetables[index];
    let mut retries = 0u32;
    loop {
        let ran = tokio::select! {
            () = shutdown.cancelled() => return,
            r = refresh_timetable_once(&urls, &kid, cache, Utc::now()) => r,
        };
        let now = Utc::now().with_timezone(&tz);
        let (wait, next_retries) = librus_next_wait(now, retries, &ran);
        retries = next_retries;
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(wait) => {}
        }
    }
}

/// Starts every fetch loop. Each runs once at startup, then on its own cadence,
/// until `shutdown` is cancelled.
pub fn spawn_all(
    cfg: Arc<Config>,
    caches: Arc<Caches>,
    sources: Sources,
    shutdown: CancellationToken,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();

    {
        let (cfg, caches, http) = (cfg.clone(), caches.clone(), sources.http.clone());
        handles.push(tokio::spawn(every(
            CALENDAR_EVERY,
            shutdown.clone(),
            move || {
                let (cfg, caches, http) = (cfg.clone(), caches.clone(), http.clone());
                async move {
                    let _ = refresh_calendar_once(
                        &http,
                        &cfg.family.ics_url,
                        &caches.calendar,
                        Utc::now(),
                    )
                    .await;
                }
            },
        )));
    }

    {
        let (cfg, caches, src) = (cfg.clone(), caches.clone(), sources.clone());
        handles.push(tokio::spawn(every(
            WEATHER_EVERY,
            shutdown.clone(),
            move || {
                let (cfg, caches, src) = (cfg.clone(), caches.clone(), src.clone());
                async move {
                    let _ = refresh_weather_once(
                        &src.http,
                        &src.weather_base,
                        &cfg.family.weather,
                        cfg.timezone,
                        &caches.weather,
                        Utc::now(),
                    )
                    .await;
                }
            },
        )));
    }

    for (index, kid) in cfg.family.kids.iter().enumerate() {
        handles.push(tokio::spawn(librus_loop(
            cfg.timezone,
            kid.clone(),
            caches.clone(),
            index,
            sources.librus.clone(),
            shutdown.clone(),
        )));
    }
    handles
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const WARSAW: Tz = chrono_tz::Europe::Warsaw;

    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Tz> {
        WARSAW.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn librus_slots() {
        assert_eq!(
            next_librus_slot(local(2026, 11, 25, 5, 0)),
            local(2026, 11, 25, 5, 30)
        );
        assert_eq!(
            next_librus_slot(local(2026, 11, 25, 5, 30)),
            local(2026, 11, 25, 7, 30)
        );
        assert_eq!(
            next_librus_slot(local(2026, 11, 25, 21, 31)),
            local(2026, 11, 26, 5, 30)
        );
    }

    #[test]
    fn librus_slots_follow_local_time_across_dst() {
        let next = next_librus_slot(local(2026, 3, 29, 1, 0));
        assert_eq!(next, local(2026, 3, 29, 5, 30));
        assert_eq!(next.to_rfc3339(), "2026-03-29T05:30:00+02:00");
        assert_eq!((next.hour(), next.minute()), (5, 30));
    }

    const FAIL: Result<(), SourceError> = Err(SourceError::Status(500));

    #[test]
    fn rejected_credentials_are_never_retried_in_the_slot() {
        let rejected = Err(SourceError::CredentialsRejected("x".into()));
        let now = local(2026, 11, 25, 5, 30);
        let (wait, n) = librus_next_wait(now, 0, &rejected);
        assert_eq!((mins(wait), n), (120, 0));
        let (wait, n) = librus_next_wait(local(2026, 11, 25, 7, 28), 1, &rejected);
        assert_eq!((mins(wait), n), (2, 0));
    }

    fn mins(d: Duration) -> u64 {
        d.as_secs() / 60
    }

    #[test]
    fn retries_follow_backoff_then_wait_for_the_slot() {
        let now = local(2026, 11, 25, 5, 30);
        assert_eq!(librus_next_wait(now, 0, &FAIL), (backoff_delay(0), 1));
        assert_eq!(librus_next_wait(now, 1, &FAIL), (backoff_delay(1), 2));
        let (wait, n) = librus_next_wait(now, 2, &FAIL);
        assert_eq!((mins(wait), n), (120, 0));
    }

    #[test]
    fn retry_is_capped_at_the_next_slot() {
        let (wait, n) = librus_next_wait(local(2026, 11, 25, 7, 28), 0, &FAIL);
        assert_eq!((mins(wait), n), (2, 0));
    }

    #[test]
    fn success_waits_for_slot_and_resets() {
        let (wait, n) = librus_next_wait(local(2026, 11, 25, 5, 31), 2, &Ok(()));
        assert_eq!((mins(wait), n), (119, 0));
    }

    #[test]
    fn failed_last_slot_retries_twice_then_waits_until_morning() {
        let mut now = local(2026, 11, 25, 21, 30);
        let mut retries = 0;
        let mut runs = vec![now];
        for _ in 0..10 {
            let (wait, n) = librus_next_wait(now, retries, &FAIL);
            retries = n;
            now += chrono::Duration::from_std(wait).unwrap();
            runs.push(now);
            if now >= local(2026, 11, 26, 5, 30) {
                break;
            }
        }
        assert_eq!(
            runs,
            vec![
                local(2026, 11, 25, 21, 30),
                local(2026, 11, 25, 21, 35),
                local(2026, 11, 25, 21, 45),
                local(2026, 11, 26, 5, 30),
            ]
        );
        assert_eq!(retries, 0);
    }

    #[test]
    fn never_more_than_two_retries_per_slot() {
        let mut now = local(2026, 11, 25, 5, 30);
        let end = local(2026, 11, 26, 5, 30);
        let mut retries = 0;
        let mut per_slot = 0;
        let mut max_per_slot = 0;
        while now < end {
            let (wait, n) = librus_next_wait(now, retries, &FAIL);
            now += chrono::Duration::from_std(wait).unwrap();
            retries = n;
            if n == 0 {
                per_slot = 0;
            } else {
                per_slot += 1;
                max_per_slot = max_per_slot.max(per_slot);
            }
        }
        assert_eq!(max_per_slot, 2);
    }

    #[tokio::test]
    async fn errors_never_leak_the_calendar_url() {
        let dir = tempfile::tempdir().unwrap();
        let cache = calendar_cache(&dir);
        let url = "http://127.0.0.1:1/SECRETMARKER/cal.ics?token=SECRETMARKER";
        let err = refresh_calendar_once(&reqwest::Client::new(), url, &cache, at(9))
            .await
            .unwrap_err();
        assert!(matches!(err, SourceError::Http(_)));
        let recorded = cache.last_error().unwrap().1;
        assert!(!recorded.contains("SECRETMARKER"), "{recorded}");
        assert!(!err.to_string().contains("SECRETMARKER"), "{err}");
        assert!(!format!("{err:?}").contains("SECRETMARKER"), "{err:?}");
    }

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 25, h, 0, 0).unwrap()
    }

    fn calendar_cache(dir: &tempfile::TempDir) -> SourceCache<IcsSnapshot> {
        SourceCache::open(dir.path().join("calendar.json"))
    }

    #[tokio::test]
    async fn calendar_304_keeps_data_and_refreshes_timestamp() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(header("if-none-match", "\"v1\""))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let cache = calendar_cache(&dir);
        let snap = IcsSnapshot {
            text: "BEGIN:VCALENDAR".into(),
            etag: Some("\"v1\"".into()),
        };
        cache.put(snap.clone(), at(8)).unwrap();

        let http = reqwest::Client::new();
        refresh_calendar_once(&http, &server.uri(), &cache, at(9))
            .await
            .unwrap();
        let got = cache.get().unwrap();
        assert_eq!(got.data, snap);
        assert_eq!(got.fetched_at, at(9));
    }

    #[tokio::test]
    async fn calendar_fresh_replaces_data_and_stores_etag() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v2\"")
                    .set_body_string("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n"),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let cache = calendar_cache(&dir);
        refresh_calendar_once(&reqwest::Client::new(), &server.uri(), &cache, at(9))
            .await
            .unwrap();
        let got = cache.get().unwrap();
        assert!(got.data.text.starts_with("BEGIN:VCALENDAR"));
        assert_eq!(got.data.etag.as_deref(), Some("\"v2\""));
    }

    #[tokio::test]
    async fn calendar_failure_keeps_data_and_records_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let cache = calendar_cache(&dir);
        let snap = IcsSnapshot {
            text: "OLD".into(),
            etag: None,
        };
        cache.put(snap.clone(), at(8)).unwrap();
        let res =
            refresh_calendar_once(&reqwest::Client::new(), &server.uri(), &cache, at(9)).await;
        assert!(res.is_err());
        assert_eq!(cache.get().unwrap().data, snap);
        assert_eq!(cache.get().unwrap().fetched_at, at(8));
        assert_eq!(cache.last_error().unwrap().0, at(9));
    }

    #[tokio::test]
    async fn calendar_garbage_200_keeps_data_and_records_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("<html><body>Sign in</body></html>"),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let cache = calendar_cache(&dir);
        let snap = IcsSnapshot {
            text: "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".into(),
            etag: None,
        };
        cache.put(snap.clone(), at(8)).unwrap();
        let res =
            refresh_calendar_once(&reqwest::Client::new(), &server.uri(), &cache, at(9)).await;
        assert!(res.is_err());
        assert_eq!(cache.get().unwrap().data, snap);
        assert_eq!(cache.get().unwrap().fetched_at, at(8));
        assert_eq!(cache.last_error().unwrap().0, at(9));
    }

    #[tokio::test]
    async fn weather_failure_keeps_data_and_records_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let cache = SourceCache::<Weather>::open(dir.path().join("weather.json"));
        let cfg = crate::config::WeatherConfig {
            latitude: 52.2,
            longitude: 21.0,
        };
        let res = refresh_weather_once(
            &reqwest::Client::new(),
            &server.uri(),
            &cfg,
            WARSAW,
            &cache,
            at(9),
        )
        .await;
        assert!(matches!(res, Err(SourceError::Status(503))));
        assert!(cache.get().is_none());
        assert!(cache.last_error().is_some());
    }
}
