//! One-shot refreshes of the family dashboard's sources.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use tracing::{info, warn};

use crate::cache::SourceCache;
use crate::family::config::KidConfig;
use crate::sources::SourceError;
use crate::sources::calendar::{self, FetchOutcome, IcsSnapshot};
use crate::sources::librus::timetable::{self, Timetable};
use crate::sources::librus::{LibrusClient, LibrusUrls};
use crate::sources::weather::{self, Weather};

/// An error message that is safe to log: reqwest errors embed the request URL,
/// which for the calendar is a secret.
pub fn describe(err: &SourceError) -> String {
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

pub fn fail<T: serde::Serialize + serde::de::DeserializeOwned + Clone>(
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

pub fn stored<T>(source: &str, result: std::io::Result<T>) {
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
    cfg: &crate::family::config::WeatherConfig,
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const WARSAW: Tz = chrono_tz::Europe::Warsaw;

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
        let cfg = crate::family::config::WeatherConfig {
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
