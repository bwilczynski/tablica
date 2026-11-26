//! One-shot refreshes of the family dashboard's sources.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use tracing::{info, warn};

use crate::family::config::KidConfig;
use crate::sources::SourceError;
use crate::sources::calendar::{self, FetchOutcome, IcsSnapshot};
use crate::sources::librus::timetable::{self, Timetable};
use crate::sources::librus::{LibrusClient, LibrusUrls};
use crate::sources::weather::{self, Weather};
use tablica_core::cache::SourceCache;

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
                SourceError::Parse("304 without cached data"),
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
    use crate::sources::test_logs::Logs;
    use chrono::TimeZone;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const WARSAW: Tz = chrono_tz::Europe::Warsaw;

    #[tokio::test]
    async fn errors_never_leak_the_calendar_url() {
        let (logs, _guard) = Logs::capture();
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
        assert!(!logs.text().contains("SECRETMARKER"), "{}", logs.text());
    }

    const SENTINEL: &str = "SECRETSENTINEL";

    /// `err` and everything derived from it must be free of the sentinel.
    fn assert_clean<T: serde::Serialize + serde::de::DeserializeOwned + Clone>(
        err: SourceError,
        cache: &SourceCache<T>,
        logs: &Logs,
    ) {
        let rendered = [
            err.to_string(),
            format!("{err:?}"),
            format!("{:#}", anyhow::anyhow!(err)),
            cache.last_error().expect("error recorded").1,
            tablica_core::cache::source_health(cache, at(9), chrono::Duration::hours(1))
                .to_string(),
            logs.text(),
        ];
        for text in rendered {
            assert!(!text.contains(SENTINEL), "{text}");
        }
    }

    // The parser is lenient: a stray line like `{SENTINEL}-line` is accepted, but
    // a mismatched `END:{SENTINEL}` makes its diagnostic quote that line.
    #[tokio::test]
    async fn malformed_ics_never_leaks_its_content() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "BEGIN:VCALENDAR\nBEGIN:VEVENT\nEND:{SENTINEL}\nEND:VCALENDAR\n"
            )))
            .mount(&server)
            .await;
        let (logs, _guard) = Logs::capture();
        let dir = tempfile::tempdir().unwrap();
        let cache = calendar_cache(&dir);
        let err = refresh_calendar_once(&reqwest::Client::new(), &server.uri(), &cache, at(9))
            .await
            .unwrap_err();
        assert!(matches!(err, SourceError::Parse(_)), "{err:?}");
        assert!(logs.text().contains("refresh failed"), "{}", logs.text());
        assert_clean(err, &cache, &logs);
    }

    #[tokio::test]
    async fn malformed_forecast_never_leaks_its_content() {
        let shapes = [
            // Wrong type where a number belongs.
            format!(r#"{{"current": "{SENTINEL}"}}"#),
            // Right shape, unparsable dates.
            format!(
                r#"{{"current": {{"temperature_2m": 1.0, "weather_code": 3}},
                    "daily": {{"time": ["{SENTINEL}", "x"], "weather_code": [3, 3],
                               "temperature_2m_max": [1.0, 1.0], "temperature_2m_min": [0.0, 0.0]}},
                    "hourly": {{"time": [], "precipitation_probability": [], "weather_code": []}}}}"#
            ),
            format!(
                r#"{{"current": {{"temperature_2m": 1.0, "weather_code": 3}},
                    "daily": {{"time": ["2026-01-05", "2026-01-06"], "weather_code": [3, 3],
                               "temperature_2m_max": [1.0, 1.0], "temperature_2m_min": [0.0, 0.0]}},
                    "hourly": {{"time": ["{SENTINEL}"], "precipitation_probability": [1.0], "weather_code": [3]}}}}"#
            ),
        ];
        for body in shapes {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
            let (logs, _guard) = Logs::capture();
            let dir = tempfile::tempdir().unwrap();
            let cache = SourceCache::<Weather>::open(dir.path().join("weather.json"));
            let cfg = crate::family::config::WeatherConfig {
                latitude: 52.2,
                longitude: 21.0,
            };
            let err = refresh_weather_once(
                &reqwest::Client::new(),
                &server.uri(),
                &cfg,
                WARSAW,
                &cache,
                at(9),
            )
            .await
            .unwrap_err();
            assert!(matches!(err, SourceError::Parse(_)), "{err:?}");
            assert_clean(err, &cache, &logs);
        }
    }

    /// Mounts steps 1 and 2 with the sentinel in the paths the responses point
    /// to: portalRodzina → `{api}/OAuth/Authorization/{SENTINEL}` → login form at
    /// `/OAuth/Authorization/{SENTINEL}/form`, where the credentials POST goes.
    async fn mount_sentinel_entry(h: &crate::sources::librus::auth::tests::Hosts) {
        Mock::given(method("GET"))
            .and(path("/loguj/portalRodzina"))
            .respond_with(ResponseTemplate::new(302).insert_header(
                "Location",
                format!(
                    "{}/OAuth/Authorization/{SENTINEL}?response_type=code",
                    h.api.uri()
                ),
            ))
            .mount(&h.synergia)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/OAuth/Authorization/{SENTINEL}")))
            .respond_with(ResponseTemplate::new(302).insert_header(
                "Location",
                format!("/OAuth/Authorization/{SENTINEL}/form?client_id=46"),
            ))
            .mount(&h.api)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/OAuth/Authorization/{SENTINEL}/form")))
            .respond_with(ResponseTemplate::new(200).set_body_string("<form></form>"))
            .mount(&h.api)
            .await;
    }

    /// The credentials POST answers with `response`.
    async fn mount_sentinel_post(
        h: &crate::sources::librus::auth::tests::Hosts,
        response: ResponseTemplate,
    ) {
        Mock::given(method("POST"))
            .and(path(format!("/OAuth/Authorization/{SENTINEL}/form")))
            .respond_with(response)
            .mount(&h.api)
            .await;
    }

    async fn refresh_kid(
        h: &crate::sources::librus::auth::tests::Hosts,
        dir: &tempfile::TempDir,
    ) -> (SourceError, SourceCache<Timetable>) {
        let cache = SourceCache::<Timetable>::open(dir.path().join("timetable-kid1.json"));
        let kid = KidConfig {
            label: "Kid".into(),
            librus_login: "login".into(),
            librus_password: "password".into(),
        };
        let err = refresh_timetable_once(&h.urls(), &kid, &cache, at(9))
            .await
            .unwrap_err();
        (err, cache)
    }

    // Hosts and paths taken from Location headers and the JSON `goTo` are
    // response-derived: they reach neither errors, the cache, health nor logs.
    #[tokio::test]
    async fn librus_response_urls_never_leak() {
        use crate::sources::librus::auth::tests::Hosts;

        // A rejected login at a login form whose path came from a redirect.
        let h = Hosts::start().await;
        mount_sentinel_entry(&h).await;
        mount_sentinel_post(&h, ResponseTemplate::new(403)).await;
        let (logs, _guard) = Logs::capture();
        let dir = tempfile::tempdir().unwrap();
        let (err, cache) = refresh_kid(&h, &dir).await;
        assert!(
            matches!(err, SourceError::CredentialsRejected(_)),
            "{err:?}"
        );
        assert!(logs.text().contains("librus login"), "{}", logs.text());
        assert_clean(err, &cache, &logs);
        drop(_guard);

        // A JSON rejection with the same response-derived login form.
        let h = Hosts::start().await;
        mount_sentinel_entry(&h).await;
        mount_sentinel_post(
            &h,
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"status": "error"})),
        )
        .await;
        let (logs, _guard) = Logs::capture();
        let dir = tempfile::tempdir().unwrap();
        let (err, cache) = refresh_kid(&h, &dir).await;
        assert!(
            matches!(err, SourceError::CredentialsRejected(_)),
            "{err:?}"
        );
        assert_clean(err, &cache, &logs);
        drop(_guard);

        // A `goTo` to a foreign host with the sentinel in host and path.
        let h = Hosts::start().await;
        mount_sentinel_entry(&h).await;
        mount_sentinel_post(
            &h,
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "goTo": format!("http://{SENTINEL}.example/{SENTINEL}/next")
            })),
        )
        .await;
        let (logs, _guard) = Logs::capture();
        let dir = tempfile::tempdir().unwrap();
        let (err, cache) = refresh_kid(&h, &dir).await;
        assert!(matches!(err, SourceError::Auth(_)), "{err:?}");
        assert_clean(err, &cache, &logs);
        drop(_guard);

        // A Location continuation on the allowed host that then fails, and one
        // that runs out of hops.
        for (hop_status, hop_location) in [(500, None), (302, Some("./loop"))] {
            let h = Hosts::start().await;
            mount_sentinel_entry(&h).await;
            mount_sentinel_post(
                &h,
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("/OAuth/Authorization/{SENTINEL}/loop")),
            )
            .await;
            let mut hop = ResponseTemplate::new(hop_status);
            if let Some(loc) = hop_location {
                hop = hop.insert_header("Location", loc);
            }
            Mock::given(method("GET"))
                .and(path(format!("/OAuth/Authorization/{SENTINEL}/loop")))
                .respond_with(hop)
                .mount(&h.api)
                .await;
            let (logs, _guard) = Logs::capture();
            let dir = tempfile::tempdir().unwrap();
            let (err, cache) = refresh_kid(&h, &dir).await;
            assert!(matches!(err, SourceError::Auth(_)), "{err:?}");
            assert_clean(err, &cache, &logs);
        }
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
