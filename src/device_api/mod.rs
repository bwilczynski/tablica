//! The TRMNL device API (BYOS protocol) plus preview and health endpoints.

pub mod images;
pub mod refresh;

use std::sync::Arc;

use anyhow::Context;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::cache::{SourceCache, is_stale};
use crate::config::{BitDepth, Config, DashboardKind, Device};
use crate::dashboards::family::{self, FamilyInputs};
use crate::dashboards::homelab;
use crate::family::{CALENDAR_MAX_AGE, FamilyCaches, TIMETABLE_MAX_AGE, WEATHER_MAX_AGE};
use crate::render::Renderer;
pub use images::ImageStore;
pub use refresh::refresh_rate;

/// Source of "now"; tests pin it.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub caches: Arc<FamilyCaches>,
    pub renderer: Arc<Renderer>,
    pub images: Arc<ImageStore>,
    pub clock: Clock,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/setup", get(setup))
        .route("/api/display", get(display))
        .route("/api/log", post(device_log))
        .route("/images/{file}", get(image))
        .route("/preview/{file}", get(preview))
        .route("/healthz", get(healthz))
        .with_state(state)
}

fn dashboard_name(kind: DashboardKind) -> &'static str {
    match kind {
        DashboardKind::Family => "family",
        DashboardKind::Homelab => "homelab",
    }
}

fn depth_number(depth: BitDepth) -> u8 {
    match depth {
        BitDepth::One => 1,
        BitDepth::Two => 2,
    }
}

/// Renders a dashboard from the cached source data as of `now`. CPU-bound.
pub fn render_dashboard(
    cfg: &Config,
    caches: &FamilyCaches,
    renderer: &Renderer,
    kind: DashboardKind,
    now: DateTime<Utc>,
    depth: BitDepth,
) -> anyhow::Result<Vec<u8>> {
    match kind {
        DashboardKind::Family => {
            let calendar = caches.calendar.get();
            let weather = caches.weather.get();
            let timetables: Vec<_> = caches.timetables.iter().map(SourceCache::get).collect();
            let view = family::build(&FamilyInputs {
                now: now.with_timezone(&cfg.timezone),
                cfg: &cfg.family,
                calendar: calendar.as_ref(),
                weather: weather.as_ref(),
                timetables: timetables.iter().map(Option::as_ref).collect(),
            });
            renderer.render("family.html", &view, depth)
        }
        DashboardKind::Homelab => renderer.render("homelab.html", &homelab::build(), depth),
    }
}

/// [`render_dashboard`] on the blocking pool, so a render never stalls the runtime.
async fn render_blocking(
    state: &AppState,
    kind: DashboardKind,
    now: DateTime<Utc>,
    depth: BitDepth,
) -> anyhow::Result<Vec<u8>> {
    let (cfg, caches, renderer) = (
        state.cfg.clone(),
        state.caches.clone(),
        state.renderer.clone(),
    );
    tokio::task::spawn_blocking(move || {
        render_dashboard(&cfg, &caches, &renderer, kind, now, depth)
    })
    .await
    .context("render task failed")?
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Uppercase hex digits only, so MACs compare regardless of case and separators.
fn mac_key(mac: &str) -> String {
    mac.chars()
        .filter(char::is_ascii_hexdigit)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// The device whose `api_key` matches the `Access-Token` header.
fn authenticate<'a>(cfg: &'a Config, headers: &HeaderMap) -> Option<&'a Device> {
    let token = header_str(headers, "Access-Token").filter(|t| !t.is_empty())?;
    cfg.devices.iter().find(|d| d.api_key == token)
}

fn base_url(cfg: &Config) -> &str {
    cfg.base_url.trim_end_matches('/')
}

fn png_response(png: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, "image/png")], png).into_response()
}

async fn setup(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let mac = header_str(&headers, "ID").unwrap_or_default();
    let wanted = mac_key(mac);
    let device = (!wanted.is_empty())
        .then(|| state.cfg.devices.iter().find(|d| mac_key(&d.mac) == wanted))
        .flatten();
    let Some(device) = device else {
        warn!(mac, "setup from unknown device");
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"status": 404, "message": "unknown device"})),
        )
            .into_response();
    };
    info!(device = %device.name, "device setup");
    let friendly_id = images::sha256_hex(device.mac.as_bytes(), 6).to_uppercase();
    Json(json!({
        "status": 200,
        "api_key": device.api_key,
        "friendly_id": friendly_id,
        "image_url": format!(
            "{}/preview/{}.png?depth={}",
            base_url(&state.cfg),
            dashboard_name(device.dashboard),
            depth_number(device.bit_depth),
        ),
        "message": format!("tablica: {}", device.name),
    }))
    .into_response()
}

async fn display(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(device) = authenticate(&state.cfg, &headers) else {
        warn!("display request with an unknown access token");
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"status": 500, "error": "Device not found", "reset_firmware": false})),
        )
            .into_response();
    };
    let h = |name| header_str(&headers, name).unwrap_or("-");
    debug!(
        device = %device.name,
        battery_voltage = h("Battery-Voltage"),
        rssi = h("RSSI"),
        fw_version = h("FW-Version"),
        model = h("Model"),
        width = h("Width"),
        height = h("Height"),
        "display request"
    );

    let now = (state.clock)();
    let kind = device.dashboard;
    let filename = match render_blocking(&state, kind, now, device.bit_depth).await {
        Ok(png) => state
            .images
            .put(&device.name, dashboard_name(kind), png, now),
        Err(e) => {
            warn!(device = %device.name, "render failed: {e:#}");
            match state.images.latest(&device.name) {
                Some(previous) => previous,
                None => {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({
                            "status": 503,
                            "error": "image not available",
                            "reset_firmware": false,
                        })),
                    )
                        .into_response();
                }
            }
        }
    };

    let switch_at = (kind == DashboardKind::Family).then_some(state.cfg.family.switch_at);
    let rate = refresh_rate(
        now.with_timezone(&state.cfg.timezone),
        &state.cfg.schedule,
        switch_at,
    );
    Json(json!({
        "status": 0,
        "image_url": format!("{}/images/{filename}.png", base_url(&state.cfg)),
        "filename": filename,
        "refresh_rate": rate,
        "reset_firmware": false,
        "update_firmware": false,
        "firmware_url": null,
        "special_function": "none",
        "maximum_compatibility": false,
        "temperature_profile": "default",
    }))
    .into_response()
}

async fn device_log(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    // Only known devices may write to the log; anyone else is turned away unlogged.
    let Some(device) = authenticate(&state.cfg, &headers) else {
        return StatusCode::UNAUTHORIZED;
    };
    let device = device.name.as_str();
    match serde_json::from_slice::<Value>(&body) {
        Ok(v) => {
            for entry in v["logs"].as_array().into_iter().flatten() {
                match &entry["message"] {
                    Value::String(msg) => info!(device, "device log: {msg}"),
                    Value::Null => info!(device, "device log: {entry}"),
                    other => info!(device, "device log: {other}"),
                }
            }
        }
        Err(_) => warn!(device, "device log body is not JSON ({} bytes)", body.len()),
    }
    StatusCode::NO_CONTENT
}

async fn image(State(state): State<AppState>, Path(file): Path<String>) -> Response {
    file.strip_suffix(".png")
        .and_then(|name| state.images.get(name))
        .map_or_else(|| StatusCode::NOT_FOUND.into_response(), png_response)
}

#[derive(Deserialize)]
struct PreviewQuery {
    at: Option<String>,
    depth: Option<u8>,
}

async fn preview(
    State(state): State<AppState>,
    Path(file): Path<String>,
    Query(q): Query<PreviewQuery>,
) -> Response {
    let kind = match file.as_str() {
        "family.png" => DashboardKind::Family,
        "homelab.png" => DashboardKind::Homelab,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let depth = match q.depth {
        None | Some(2) => BitDepth::Two,
        Some(1) => BitDepth::One,
        Some(_) => return (StatusCode::BAD_REQUEST, "depth must be 1 or 2").into_response(),
    };
    let now = match q.at {
        None => (state.clock)(),
        // An unencoded `+` in the offset arrives as a space.
        Some(at) => match DateTime::parse_from_rfc3339(&at.replace(' ', "+")) {
            Ok(t) => t.with_timezone(&Utc),
            Err(_) => {
                return (StatusCode::BAD_REQUEST, "at must be an RFC 3339 timestamp")
                    .into_response();
            }
        },
    };
    match render_blocking(&state, kind, now, depth).await {
        Ok(png) => png_response(png),
        Err(e) => {
            warn!("preview render failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn source_health<T: serde::Serialize + DeserializeOwned + Clone>(
    cache: &SourceCache<T>,
    now: DateTime<Utc>,
    max_age: Duration,
) -> Value {
    let fetched_at = cache.get().map(|s| s.fetched_at);
    json!({
        "fetched_at": fetched_at.map(|t| t.to_rfc3339()),
        "stale": fetched_at.is_none_or(|t| is_stale(t, now, max_age)),
        "last_error": cache.last_error().map(|(_, msg)| msg),
    })
}

async fn healthz(State(state): State<AppState>) -> Json<Value> {
    let now = (state.clock)();
    let caches = &state.caches;
    Json(json!({
        "status": "ok",
        "sources": {
            "calendar": source_health(&caches.calendar, now, CALENDAR_MAX_AGE),
            "weather": source_health(&caches.weather, now, WEATHER_MAX_AGE),
            "timetables": caches
                .timetables
                .iter()
                .map(|c| source_health(c, now, TIMETABLE_MAX_AGE))
                .collect::<Vec<_>>(),
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::config::{BitDepth, DashboardKind, Device, Schedule};
    use crate::dashboards::test_support::{sample_inputs, time, warsaw};
    use crate::render::TemplateSource;

    const BASE: &str = "http://tablica.test:8080";

    struct Fixture {
        _dir: tempfile::TempDir,
        state: AppState,
    }

    fn device(
        name: &str,
        mac: &str,
        key: &str,
        dashboard: DashboardKind,
        depth: BitDepth,
    ) -> Device {
        Device {
            name: name.into(),
            mac: mac.into(),
            api_key: key.into(),
            dashboard,
            bit_depth: depth,
        }
    }

    /// Seeded caches and a clock pinned to Wednesday 2026-11-25 07:00 Warsaw.
    fn fixture_with(templates: TemplateSource) -> Fixture {
        let now = warsaw(2026, 11, 25, 7, 0);
        let dir = tempfile::tempdir().unwrap();
        let sample = sample_inputs(now);
        let caches = FamilyCaches::open(dir.path(), 2);
        if let Some(c) = sample.calendar {
            caches.calendar.put(c.data, c.fetched_at).unwrap();
        }
        if let Some(w) = sample.weather {
            caches.weather.put(w.data, w.fetched_at).unwrap();
        }
        for (cache, tt) in caches.timetables.iter().zip(sample.timetables) {
            if let Some(tt) = tt {
                cache.put(tt.data, tt.fetched_at).unwrap();
            }
        }
        let cfg = Config {
            timezone: chrono_tz::Europe::Warsaw,
            base_url: format!("{BASE}/"),
            listen: "127.0.0.1:0".parse().unwrap(),
            data_dir: dir.path().to_owned(),
            schedule: Schedule {
                wake: time(6, 0),
                sleep: time(22, 0),
                refresh_minutes: 15,
            },
            devices: vec![
                device(
                    "kitchen",
                    "AA:BB:CC:DD:EE:01",
                    "kitchen-key",
                    DashboardKind::Family,
                    BitDepth::Two,
                ),
                device(
                    "office",
                    "AA:BB:CC:DD:EE:02",
                    "office-key",
                    DashboardKind::Homelab,
                    BitDepth::One,
                ),
            ],
            family: sample.cfg,
        };
        let fixed = now.with_timezone(&Utc);
        Fixture {
            _dir: dir,
            state: AppState {
                cfg: Arc::new(cfg),
                caches: Arc::new(caches),
                renderer: Arc::new(Renderer::new(templates).unwrap()),
                images: Arc::new(ImageStore::new()),
                clock: Arc::new(move || fixed),
            },
        }
    }

    fn fixture() -> Fixture {
        fixture_with(TemplateSource::Embedded)
    }

    fn get(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
        let mut req = Request::get(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        req.body(Body::empty()).unwrap()
    }

    async fn send(state: &AppState, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        let (parts, body) = resp.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes().to_vec();
        (parts.status, parts.headers, bytes)
    }

    async fn send_json(state: &AppState, req: Request<Body>) -> (StatusCode, Value) {
        let (status, _, body) = send(state, req).await;
        let json = serde_json::from_slice(&body)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&body)));
        (status, json)
    }

    fn png_depth(bytes: &[u8]) -> png::BitDepth {
        let r = png::Decoder::new(std::io::Cursor::new(bytes))
            .read_info()
            .unwrap();
        assert_eq!((r.info().width, r.info().height), (800, 480));
        r.info().bit_depth
    }

    #[tokio::test]
    async fn setup_known_and_unknown_mac() {
        let f = fixture();
        let (status, body) =
            send_json(&f.state, get("/api/setup", &[("ID", "aa:bb:cc:dd:ee:01")])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], 200);
        assert_eq!(body["api_key"], "kitchen-key");
        assert_eq!(body["message"], "tablica: kitchen");
        let expected = images::sha256_hex(b"AA:BB:CC:DD:EE:01", 6).to_uppercase();
        assert_eq!(body["friendly_id"], expected.as_str());
        assert!(
            body["image_url"].as_str().unwrap().starts_with(BASE),
            "{body}"
        );
        assert!(!body["image_url"].as_str().unwrap().contains("//preview"));

        for headers in [&[("ID", "AA:BB:CC:DD:EE:99")][..], &[]] {
            let (status, body) = send_json(&f.state, get("/api/setup", headers)).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(
                body,
                serde_json::json!({"status": 404, "message": "unknown device"})
            );
        }
    }

    #[tokio::test]
    async fn display_rejects_bad_token_without_reset() {
        let f = fixture();
        for headers in [
            &[("Access-Token", "wrong")][..],
            &[("Access-Token", "")],
            &[],
        ] {
            let (status, body) = send_json(&f.state, get("/api/display", headers)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            assert_eq!(body["reset_firmware"], false);
            assert_eq!(body["status"], 500);
            assert_eq!(body["error"], "Device not found");
        }
    }

    #[tokio::test]
    async fn display_returns_numeric_refresh_rate_and_stable_filename() {
        let f = fixture();
        let req = || get("/api/display", &[("Access-Token", "kitchen-key")]);
        let (status, first) = send_json(&f.state, req()).await;
        assert_eq!(status, StatusCode::OK);
        let (_, second) = send_json(&f.state, req()).await;
        assert_eq!(first["filename"], second["filename"]);

        let filename = first["filename"].as_str().unwrap();
        assert!(filename.len() <= 31, "{filename}");
        assert!(filename.starts_with("family-"), "{filename}");
        assert!(first["refresh_rate"].is_u64(), "{first}");
        assert_eq!(first["refresh_rate"], 900);
        assert_eq!(first["status"], 0);
        assert_eq!(first["reset_firmware"], false);
        assert_eq!(first["update_firmware"], false);
        assert_eq!(first["firmware_url"], Value::Null);
        assert_eq!(first["special_function"], "none");
        assert_eq!(first["maximum_compatibility"], false);
        assert_eq!(first["temperature_profile"], "default");
        assert_eq!(
            first["image_url"],
            format!("{BASE}/images/{filename}.png").as_str()
        );

        let (status, office) = send_json(
            &f.state,
            get("/api/display", &[("Access-Token", "office-key")]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(office["filename"].as_str().unwrap().starts_with("homelab-"));
        assert!(office["filename"].as_str().unwrap().len() <= 31);
        assert_eq!(office["reset_firmware"], false);
    }

    #[tokio::test]
    async fn display_serves_png_at_image_url() {
        let f = fixture();
        for (key, depth) in [
            ("kitchen-key", png::BitDepth::Two),
            ("office-key", png::BitDepth::One),
        ] {
            let (_, body) =
                send_json(&f.state, get("/api/display", &[("Access-Token", key)])).await;
            let url = body["image_url"].as_str().unwrap();
            let path = url.strip_prefix(BASE).unwrap();
            let (status, headers, png) = send(&f.state, get(path, &[])).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers[header::CONTENT_TYPE], "image/png");
            assert_eq!(png_depth(&png), depth);
        }
        let (status, _, _) = send(&f.state, get("/images/family-00000000-1.png", &[])).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn display_reuses_previous_filename_when_render_fails() {
        let templates = tempfile::tempdir().unwrap();
        copy_dir(Path::new("templates"), templates.path());
        let f = fixture_with(TemplateSource::Dir(templates.path().to_owned()));
        let kitchen = || get("/api/display", &[("Access-Token", "kitchen-key")]);
        let (status, good) = send_json(&f.state, kitchen()).await;
        assert_eq!(status, StatusCode::OK);

        std::fs::write(templates.path().join("family.html"), "{% if %}broken").unwrap();
        std::fs::write(templates.path().join("homelab.html"), "{% if %}broken").unwrap();
        let (status, again) = send_json(&f.state, kitchen()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(again["filename"], good["filename"]);
        assert_eq!(again["reset_firmware"], false);

        let (status, _, _) = send(
            &f.state,
            get("/api/display", &[("Access-Token", "office-key")]),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn preview_honors_at_and_depth() {
        let f = fixture();
        let fetch = |uri: &'static str| {
            let state = f.state.clone();
            async move {
                let (status, headers, body) = send(&state, get(uri, &[])).await;
                assert_eq!(status, StatusCode::OK, "{uri}");
                assert_eq!(headers[header::CONTENT_TYPE], "image/png");
                body
            }
        };
        let morning = fetch("/preview/family.png?at=2026-11-25T07:00:00%2B01:00").await;
        assert_eq!(png_depth(&morning), png::BitDepth::Two);
        // An unencoded `+` arrives as a space; it still parses.
        let morning_raw = fetch("/preview/family.png?at=2026-11-25T07:00:00+01:00").await;
        assert_eq!(morning, morning_raw);
        // No `at` means the clock, which is pinned to that same morning.
        assert_eq!(fetch("/preview/family.png").await, morning);
        let evening = fetch("/preview/family.png?at=2026-11-25T19:00:00%2B01:00").await;
        assert_ne!(morning, evening);
        let one = fetch("/preview/family.png?at=2026-11-25T07:00:00%2B01:00&depth=1").await;
        assert_eq!(png_depth(&one), png::BitDepth::One);
        let homelab = fetch("/preview/homelab.png?depth=2").await;
        assert_eq!(png_depth(&homelab), png::BitDepth::Two);

        // Preview never stores images.
        assert!(f.state.images.latest("kitchen").is_none());

        for (uri, code) in [
            ("/preview/other.png", StatusCode::NOT_FOUND),
            ("/preview/family.png?depth=3", StatusCode::BAD_REQUEST),
            ("/preview/family.png?at=yesterday", StatusCode::BAD_REQUEST),
        ] {
            let (status, _, _) = send(&f.state, get(uri, &[])).await;
            assert_eq!(status, code, "{uri}");
        }
    }

    #[tokio::test]
    async fn log_requires_a_device_token() {
        let f = fixture();
        let body =
            r#"{"logs":[{"message":"wifi connected","level":"info"},{"message":"battery low"}]}"#;
        for (headers, body, code) in [
            (
                vec![("Access-Token", "kitchen-key")],
                body,
                StatusCode::NO_CONTENT,
            ),
            (
                vec![("Access-Token", "kitchen-key")],
                "not json",
                StatusCode::NO_CONTENT,
            ),
            (vec![], body, StatusCode::UNAUTHORIZED),
            (
                vec![("Access-Token", "wrong")],
                "not json",
                StatusCode::UNAUTHORIZED,
            ),
        ] {
            let mut req =
                Request::post("/api/log").header(header::CONTENT_TYPE, "application/json");
            for (k, v) in headers {
                req = req.header(k, v);
            }
            let (status, _, _) = send(&f.state, req.body(Body::from(body)).unwrap()).await;
            assert_eq!(status, code, "{body}");
        }
    }

    #[tokio::test]
    async fn healthz_reports_staleness() {
        let f = fixture();
        let now = (f.state.clock)();
        let weather = f.state.caches.weather.get().unwrap().data;
        f.state
            .caches
            .weather
            .put(weather, now - chrono::Duration::hours(4))
            .unwrap();
        f.state
            .caches
            .calendar
            .record_error("http error: timeout".into(), now);

        let (status, body) = send_json(&f.state, get("/healthz", &[])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        let sources = &body["sources"];
        assert_eq!(sources["calendar"]["stale"], false);
        assert_eq!(sources["calendar"]["last_error"], "http error: timeout");
        assert!(sources["calendar"]["fetched_at"].is_string());
        assert_eq!(sources["weather"]["stale"], true);
        assert_eq!(sources["weather"]["last_error"], Value::Null);
        let timetables = sources["timetables"].as_array().unwrap();
        assert_eq!(timetables.len(), 2);
        assert_eq!(timetables[0]["stale"], false);

        // An empty cache is stale with no timestamp.
        let empty = tempfile::tempdir().unwrap();
        let state = AppState {
            caches: Arc::new(FamilyCaches::open(empty.path(), 2)),
            ..f.state.clone()
        };
        let (_, body) = send_json(&state, get("/healthz", &[])).await;
        assert_eq!(body["sources"]["weather"]["stale"], true);
        assert_eq!(body["sources"]["weather"]["fetched_at"], Value::Null);
        assert_eq!(body["sources"]["timetables"][1]["stale"], true);
    }
}
