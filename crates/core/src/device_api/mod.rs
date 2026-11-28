//! The TRMNL device API (BYOS protocol) plus preview and health endpoints.

mod access_log;
pub mod images;
pub mod refresh;
#[cfg(test)]
pub(crate) mod stub;
mod telemetry;

use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::config::{BitDepth, Config, Device};
use crate::dashboard::Active;
use crate::diagnostics::view::{DiagInput, build};
use crate::history::{History, READ_DEADLINE};
use crate::metrics::{Recorder, RenderKind};
use crate::render::Renderer;
use access_log::Access;
pub use images::ImageStore;
pub use refresh::refresh_rate;

/// Source of "now"; tests pin it.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub dashboards: Arc<Active>,
    pub renderer: Arc<Renderer>,
    pub images: Arc<ImageStore>,
    pub clock: Clock,
    pub history: Arc<History>,
    /// Serialises diagnostics history reads: at most one at a time.
    pub diag_reads: Arc<Semaphore>,
    /// Prometheus instruments; recording never fails or waits on I/O.
    pub recorder: Recorder,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/setup", get(setup))
        .route("/api/display", get(display))
        .route("/api/log", post(device_log))
        .route("/images/{file}", get(image))
        .route("/preview/{file}", get(preview))
        .route("/healthz", get(healthz))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            access_log::layer,
        ))
        .with_state(state)
}

fn depth_number(depth: BitDepth) -> u8 {
    match depth {
        BitDepth::One => 1,
        BitDepth::Two => 2,
    }
}

/// Renders dashboard `name` from its cached data as of `now`. CPU-bound.
pub fn render_dashboard(
    state: &AppState,
    name: &str,
    now: DateTime<Utc>,
    depth: BitDepth,
) -> anyhow::Result<Vec<u8>> {
    let dashboard = state
        .dashboards
        .get(name)
        .with_context(|| format!("dashboard `{name}` is not active"))?;
    let view = dashboard.view(now.with_timezone(&state.cfg.timezone))?;
    state.renderer.render(name, &view, depth)
}

/// [`render_dashboard`] on the blocking pool, so a render never stalls the runtime.
async fn render_blocking(
    state: &AppState,
    name: &str,
    now: DateTime<Utc>,
    depth: BitDepth,
    kind: RenderKind,
) -> anyhow::Result<Vec<u8>> {
    let started = Instant::now();
    let (task_state, task_name) = (state.clone(), name.to_owned());
    let result =
        tokio::task::spawn_blocking(move || render_dashboard(&task_state, &task_name, now, depth))
            .await
            .context("render task failed")
            .and_then(|r| r);
    state
        .recorder
        .render(name, kind, started.elapsed(), result.is_ok());
    result
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

/// The configured device with this MAC (any case or separators), if any.
fn device_by_mac<'a>(cfg: &'a Config, mac: &str) -> Option<&'a Device> {
    let wanted = mac_key(mac);
    (!wanted.is_empty())
        .then(|| cfg.devices.iter().find(|d| mac_key(&d.mac) == wanted))
        .flatten()
}

fn base_url(cfg: &Config) -> &str {
    cfg.base_url.trim_end_matches('/')
}

/// `response` with `note` attached for the access log.
fn noted(mut response: Response, note: Access) -> Response {
    response.extensions_mut().insert(note);
    response
}

fn png_response(png: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, "image/png")], png).into_response()
}

async fn setup(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let mac = header_str(&headers, "ID").unwrap_or_default();
    let Some(device) = device_by_mac(&state.cfg, mac) else {
        let response = (
            StatusCode::NOT_FOUND,
            Json(json!({"status": 404, "message": "unknown device"})),
        )
            .into_response();
        return noted(response, Access::refused("unknown device"));
    };
    let friendly_id = images::sha256_hex(device.mac.as_bytes(), 6).to_uppercase();
    let image_url = format!(
        "{}/preview/{}.png?depth={}",
        base_url(&state.cfg),
        device.dashboard,
        depth_number(device.bit_depth),
    );
    let response = Json(json!({
        "status": 200,
        "api_key": device.api_key,
        "friendly_id": friendly_id,
        "image_url": image_url,
        "message": format!("tablica: {}", device.name),
    }))
    .into_response();
    noted(response, Access::served(&device.name, Some(image_url)))
}

async fn display(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(device) = authenticate(&state.cfg, &headers) else {
        let response = (
            StatusCode::UNAUTHORIZED,
            Json(json!({"status": 500, "error": "Device not found", "reset_firmware": false})),
        )
            .into_response();
        return noted(response, unknown_token(&state.cfg, &headers));
    };
    let now = (state.clock)();
    let dashboard = device.dashboard.as_str();
    let mut reading = telemetry::reading(&device.mac, now, &headers);
    let started = Instant::now();
    let rendered =
        render_blocking(&state, dashboard, now, device.bit_depth, RenderKind::Device).await;
    if rendered.is_ok() {
        reading.render_ms = Some(i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX));
    }
    let filename = match rendered {
        Ok(png) => {
            reading.image_bytes = i64::try_from(png.len()).ok();
            state.images.put(&device.name, dashboard, png, now)
        }
        Err(e) => {
            warn!(device = %device.name, "render failed: {e:#}");
            match state.images.latest(&device.name) {
                Some(previous) => previous,
                None => {
                    state.recorder.record_reading(&device.name, &reading);
                    state.history.record_reading(reading);
                    let response = (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({
                            "status": 503,
                            "error": "image not available",
                            "reset_firmware": false,
                        })),
                    )
                        .into_response();
                    let note =
                        Access::unavailable(&device.name, "render failed and no previous image");
                    return noted(response, note);
                }
            }
        }
    };

    let switch_at = state.dashboards.get(dashboard).and_then(|d| d.switch_at());
    let rate = refresh_rate(
        now.with_timezone(&state.cfg.timezone),
        &state.cfg.schedule,
        switch_at,
    );
    let image_url = format!("{}/images/{filename}.png", base_url(&state.cfg));
    reading.next_refresh = Some(i64::from(rate));
    state.recorder.record_reading(&device.name, &reading);
    state.history.record_reading(reading);
    let response = Json(json!({
        "status": 0,
        "image_url": image_url,
        "filename": filename,
        "refresh_rate": rate,
        "reset_firmware": false,
        "update_firmware": false,
        "firmware_url": null,
        "special_function": "none",
        "maximum_compatibility": false,
        "temperature_profile": "default",
    }))
    .into_response();
    noted(response, Access::served(&device.name, Some(image_url)))
}

/// The access-log note for a request whose `Access-Token` matches no device. The
/// MAC tells a configured device holding a stale key (e.g. one issued by another
/// server) apart from a stranger.
fn unknown_token(cfg: &Config, headers: &HeaderMap) -> Access {
    let mac = header_str(headers, "ID").unwrap_or_default();
    Access::refused(match device_by_mac(cfg, mac) {
        Some(d) => format!("unknown access token; ID is configured device {}", d.name),
        None => "unknown access token".to_owned(),
    })
}

async fn device_log(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    // Only known devices may write to the log; anyone else gets only an access line.
    let Some(device) = authenticate(&state.cfg, &headers) else {
        let response = StatusCode::UNAUTHORIZED.into_response();
        return noted(response, unknown_token(&state.cfg, &headers));
    };
    let note = Access::served(&device.name, None);
    state
        .history
        .record_log_body(&device.mac, (state.clock)(), &body);
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
    noted(StatusCode::NO_CONTENT.into_response(), note)
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
    device: Option<String>,
}

/// The `depth` query value, or a 400 response.
fn parse_depth(depth: Option<u8>) -> Result<BitDepth, (StatusCode, &'static str)> {
    match depth {
        None | Some(2) => Ok(BitDepth::Two),
        Some(1) => Ok(BitDepth::One),
        Some(_) => Err((StatusCode::BAD_REQUEST, "depth must be 1 or 2")),
    }
}

/// The `at` query value, or a 400 response.
fn parse_at(at: Option<String>) -> Result<Option<DateTime<Utc>>, (StatusCode, &'static str)> {
    let Some(at) = at else { return Ok(None) };
    // An unencoded `+` in the offset arrives as a space.
    DateTime::parse_from_rfc3339(&at.replace(' ', "+"))
        .map(|t| Some(t.with_timezone(&Utc)))
        .map_err(|_| (StatusCode::BAD_REQUEST, "at must be an RFC 3339 timestamp"))
}

async fn preview(
    State(state): State<AppState>,
    Path(file): Path<String>,
    Query(q): Query<PreviewQuery>,
) -> Response {
    if file == "diag.png" {
        return diag_preview(&state, q).await;
    }
    let Some(dashboard) = file
        .strip_suffix(".png")
        .filter(|name| state.dashboards.contains_key(name))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let depth = match parse_depth(q.depth) {
        Ok(d) => d,
        Err(e) => return e.into_response(),
    };
    let now = match parse_at(q.at) {
        Ok(at) => at.unwrap_or_else(|| (state.clock)()),
        Err(e) => return e.into_response(),
    };
    match render_blocking(&state, dashboard, now, depth, RenderKind::Preview).await {
        Ok(png) => png_response(png),
        Err(e) => {
            warn!("preview render failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Reads the history and dashboard sections for `device`'s diagnostics screen.
/// `at` moves only the history; dashboards always get the real "now".
pub(crate) async fn diag_input<'a>(
    state: &AppState,
    device: &'a Device,
    at: Option<DateTime<Utc>>,
) -> DiagInput<'a> {
    let now = (state.clock)();
    let moment = at.unwrap_or(now);
    let history = read_history(state, &device.mac, moment).await;
    let sections = state
        .dashboards
        .get(device.dashboard.as_str())
        .map(|d| d.diagnostics(now))
        .unwrap_or_default();
    DiagInput {
        device,
        history,
        sections,
        at: moment,
        at_given: at.is_some(),
        tz: state.cfg.timezone,
    }
}

/// One history snapshot, at most one at a time, within the read deadline. A read
/// still in flight (even a stuck one) makes this one "unavailable" at once
/// rather than queueing behind it.
///
/// The read runs on its own detached thread, not Tokio's blocking pool: the
/// runtime waits for blocking-pool tasks when it shuts down, so a stalled read
/// there could hold up process exit.
async fn read_history(
    state: &AppState,
    mac: &str,
    at: DateTime<Utc>,
) -> anyhow::Result<crate::history::DeviceHistory> {
    let permit = state
        .diag_reads
        .clone()
        .try_acquire_owned()
        .context("another history read is in progress")?;
    let (history, mac) = (state.history.clone(), mac.to_owned());
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("history-read".into())
        .spawn(move || {
            let result = history.snapshot(&mac, at);
            drop(permit);
            let _ = tx.send(result);
        })
        .context("cannot start the history read")?;
    tokio::time::timeout(READ_DEADLINE + std::time::Duration::from_secs(1), rx)
        .await
        .context("history read timed out")?
        .context("history read ended without a result")?
}

async fn diag_preview(state: &AppState, q: PreviewQuery) -> Response {
    let Some(name) = q.device.as_deref() else {
        return (StatusCode::BAD_REQUEST, "device is required").into_response();
    };
    let Some(device) = state.cfg.devices.iter().find(|d| d.name == name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let depth = match parse_depth(q.depth) {
        Ok(d) => d,
        Err(e) => return e.into_response(),
    };
    let at = match parse_at(q.at) {
        Ok(at) => at,
        Err(e) => return e.into_response(),
    };
    let view = build(diag_input(state, device, at).await);
    let renderer = state.renderer.clone();
    let started = Instant::now();
    let rendered = tokio::task::spawn_blocking(move || renderer.render_diag(&view, depth)).await;
    state.recorder.render(
        &device.dashboard,
        RenderKind::Diag,
        started.elapsed(),
        matches!(rendered, Ok(Ok(_))),
    );
    match rendered {
        Ok(Ok(png)) => png_response(png),
        Ok(Err(e)) => {
            warn!("diagnostics render failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(e) => {
            warn!("diagnostics render task failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn healthz(State(state): State<AppState>) -> Json<Value> {
    let now = (state.clock)();
    let dashboards: serde_json::Map<String, Value> = state
        .dashboards
        .iter()
        .map(|(&name, d)| (name.to_owned(), d.health(now)))
        .collect();
    Json(json!({ "status": "ok", "dashboards": dashboards }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode, header};
    use chrono::{NaiveTime, TimeZone};
    use chrono_tz::Europe::Warsaw;
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::config::{BitDepth, Device, Schedule};
    use crate::dashboard::Dashboard;
    use crate::history::History;
    use crate::render::TemplateSource;
    use stub::StubDashboard;

    const BASE: &str = "http://tablica.test:8080";

    fn time(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn warsaw(h: u32, m: u32) -> DateTime<Utc> {
        Warsaw
            .with_ymd_and_hms(2026, 11, 25, h, m, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    struct Fixture {
        state: AppState,
        alpha: Arc<StubDashboard>,
    }

    fn device(name: &str, mac: &str, key: &str, dashboard: &str, depth: BitDepth) -> Device {
        Device {
            name: name.into(),
            mac: mac.into(),
            api_key: key.into(),
            dashboard: dashboard.into(),
            bit_depth: depth,
        }
    }

    /// Two stub dashboards and a clock pinned to Wednesday 2026-11-25 `h:m` Warsaw.
    /// `kitchen` shows `alpha` (switches at 18:00) at 2 bits, `office` `bravo` at 1 bit.
    fn fixture_at(h: u32, m: u32) -> Fixture {
        fixture_with(h, m, Arc::new(History::disabled()), std::env::temp_dir())
    }

    fn fixture_with(
        h: u32,
        m: u32,
        history: Arc<History>,
        data_dir: std::path::PathBuf,
    ) -> Fixture {
        let alpha = Arc::new(StubDashboard {
            switch_at: Some(time(18, 0)),
            ..StubDashboard::new("alpha")
        });
        let bravo = Arc::new(StubDashboard::new("bravo"));
        let dashboards = Active::from([
            ("alpha", alpha.clone() as Arc<dyn Dashboard>),
            ("bravo", bravo as Arc<dyn Dashboard>),
        ]);
        let cfg = Config {
            timezone: Warsaw,
            base_url: format!("{BASE}/"),
            listen: "127.0.0.1:0".parse().unwrap(),
            metrics_listen: None,
            data_dir,
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
                    "alpha",
                    BitDepth::Two,
                ),
                device(
                    "office",
                    "AA:BB:CC:DD:EE:02",
                    "office-key",
                    "bravo",
                    BitDepth::One,
                ),
            ],
        };
        let renderer = Renderer::new(TemplateSource::Embedded, &dashboards).unwrap();
        let fixed = warsaw(h, m);
        Fixture {
            state: AppState {
                cfg: Arc::new(cfg),
                dashboards: Arc::new(dashboards),
                renderer: Arc::new(renderer),
                images: Arc::new(ImageStore::new()),
                clock: Arc::new(move || fixed),
                history,
                diag_reads: Arc::new(Semaphore::new(1)),
                recorder: Recorder::new(&[
                    ("kitchen".to_owned(), "alpha".to_owned()),
                    ("office".to_owned(), "bravo".to_owned()),
                ]),
            },
            alpha,
        }
    }

    fn fixture() -> Fixture {
        fixture_at(7, 0)
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
    async fn setup_image_url_uses_dashboard_name_and_depth() {
        let f = fixture();
        let (_, kitchen) =
            send_json(&f.state, get("/api/setup", &[("ID", "AA:BB:CC:DD:EE:01")])).await;
        assert_eq!(
            kitchen["image_url"],
            format!("{BASE}/preview/alpha.png?depth=2").as_str()
        );
        let (_, office) =
            send_json(&f.state, get("/api/setup", &[("ID", "AA:BB:CC:DD:EE:02")])).await;
        assert_eq!(
            office["image_url"],
            format!("{BASE}/preview/bravo.png?depth=1").as_str()
        );
    }

    fn open_history(dir: &std::path::Path, at: DateTime<Utc>) -> Arc<History> {
        Arc::new(History::open(dir, Arc::new(move || at)))
    }

    const KITCHEN_MAC: &str = "AA:BB:CC:DD:EE:01";

    fn reading_at(mac: &str, at: DateTime<Utc>) -> crate::history::Reading {
        crate::history::Reading {
            device: mac.into(),
            at,
            battery_v: Some(4.0),
            rssi: Some(-60),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn diag_preview_requires_known_device() {
        let f = fixture();
        for (uri, want) in [
            ("/preview/diag.png", StatusCode::BAD_REQUEST),
            ("/preview/diag.png?device=nobody", StatusCode::NOT_FOUND),
            (
                "/preview/diag.png?device=kitchen&depth=3",
                StatusCode::BAD_REQUEST,
            ),
            (
                "/preview/diag.png?device=kitchen&at=nope",
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let (status, _, _) = send(&f.state, get(uri, &[])).await;
            assert_eq!(status, want, "{uri}");
        }
        let (_, _, body) = send(&f.state, get("/preview/diag.png", &[])).await;
        assert_eq!(body, b"device is required");
    }

    #[tokio::test]
    async fn diag_preview_renders_png() {
        let dir = tempfile::tempdir().unwrap();
        let history = open_history(dir.path(), warsaw(7, 0));
        let f = fixture_with(7, 0, history.clone(), dir.path().to_owned());
        let (status, _, _) = send(
            &f.state,
            get("/api/display", &[("Access-Token", "kitchen-key")]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        history.flush();
        let (status, headers, body) =
            send(&f.state, get("/preview/diag.png?device=kitchen", &[])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "image/png");
        let info = png::Decoder::new(std::io::Cursor::new(&body))
            .read_info()
            .unwrap();
        assert_eq!(
            (info.info().width, info.info().height, info.info().bit_depth),
            (800, 480, png::BitDepth::Two)
        );
        let (status, _, body) = send(
            &f.state,
            get("/preview/diag.png?device=kitchen&depth=1", &[]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(png_depth(&body), png::BitDepth::One);
    }

    #[tokio::test]
    async fn diag_input_filters_history_by_at() {
        let dir = tempfile::tempdir().unwrap();
        let history = open_history(dir.path(), warsaw(7, 0));
        history.record_reading(reading_at(KITCHEN_MAC, warsaw(6, 0)));
        history.record_reading(reading_at(KITCHEN_MAC, warsaw(7, 0)));
        history.flush();
        let f = fixture_with(8, 0, history, dir.path().to_owned());
        let device = &f.state.cfg.devices[0];
        let latest = |at: DateTime<Utc>| {
            let state = f.state.clone();
            async move {
                let input = diag_input(&state, device, Some(at)).await;
                assert!(input.at_given);
                assert_eq!(input.at, at);
                input.history.unwrap().latest.map(|r| r.at)
            }
        };
        assert_eq!(latest(warsaw(6, 30)).await, Some(warsaw(6, 0)));
        let year = chrono::Duration::days(365);
        assert_eq!(latest(warsaw(6, 30) - year).await, None);
        assert_eq!(latest(warsaw(6, 30) + year).await, Some(warsaw(7, 0)));
        let input = diag_input(&f.state, device, None).await;
        assert!(!input.at_given);
        assert_eq!(input.at, warsaw(8, 0));
    }

    #[tokio::test]
    async fn diag_input_passes_real_now_to_dashboards() {
        let f = fixture_at(7, 0);
        let device = &f.state.cfg.devices[0];
        let earlier = warsaw(3, 0);
        let input = diag_input(&f.state, device, Some(earlier)).await;
        assert!(input.sections.is_empty());
        assert_eq!(*f.alpha.diag_now.lock().unwrap(), Some(warsaw(7, 0)));
    }

    #[tokio::test]
    async fn busy_history_read_is_unavailable_not_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        let history = open_history(dir.path(), warsaw(7, 0));
        history.record_reading(reading_at(KITCHEN_MAC, warsaw(6, 0)));
        history.flush();
        let f = fixture_with(7, 0, history, dir.path().to_owned());
        let _held = f.state.diag_reads.clone().try_acquire_owned().unwrap();
        let quick = std::time::Duration::from_millis(500);

        let device = &f.state.cfg.devices[0];
        let input = tokio::time::timeout(quick, diag_input(&f.state, device, None))
            .await
            .expect("diag_input waited for the busy permit");
        assert!(input.history.is_err());

        let (status, headers, body) = tokio::time::timeout(
            quick,
            send(&f.state, get("/preview/diag.png?device=kitchen", &[])),
        )
        .await
        .expect("diag preview waited for the busy permit");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "image/png");
        assert_eq!(png_depth(&body), png::BitDepth::Two);
    }

    /// Paused time: the clock jumps once the runtime is idle, so waiting out
    /// the deadline is instant, while a read on Tokio's blocking pool would
    /// hold the clock (and the runtime's exit) until it finished. The body runs
    /// on its own thread under a real-time watchdog, so that regression fails
    /// the test instead of hanging it.
    #[test]
    fn stalled_history_read_is_unavailable_within_the_deadline() {
        const WATCHDOG: std::time::Duration = std::time::Duration::from_secs(10);
        let (finished, done) = std::sync::mpsc::channel();
        let body = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .start_paused(true)
                .build()
                .unwrap()
                .block_on(stalled_read_body());
            let _ = finished.send(());
        });
        match done.recv_timeout(WATCHDOG) {
            // Disconnected: the body panicked; join to report its panic.
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if let Err(panic) = body.join() {
                    std::panic::resume_unwind(panic);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!(
                "a stalled history read hung the handler for {WATCHDOG:?}: \
                 is it back on Tokio's blocking pool?"
            ),
        }
    }

    async fn stalled_read_body() {
        let dir = tempfile::tempdir().unwrap();
        let history = open_history(dir.path(), warsaw(7, 0));
        history.record_reading(reading_at(KITCHEN_MAC, warsaw(6, 0)));
        history.flush();
        let f = fixture_with(7, 0, history.clone(), dir.path().to_owned());
        let device = &f.state.cfg.devices[0];
        let limit = READ_DEADLINE + std::time::Duration::from_secs(1);

        let release = history.stall_next_read();
        let start = tokio::time::Instant::now();
        let input = diag_input(&f.state, device, None).await;
        assert!(input.history.is_err());
        assert!(start.elapsed() <= limit, "{:?}", start.elapsed());
        // The stalled read still holds the permit, so the next one is refused.
        assert_eq!(f.state.diag_reads.available_permits(), 0);

        drop(release);
        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let permit = loop {
            match f.state.diag_reads.clone().try_acquire_owned() {
                Ok(p) => break p,
                Err(_) if std::time::Instant::now() < give_up => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(_) => panic!("the released read never gave its permit back"),
            }
        };
        drop(permit);

        let release = history.stall_next_read();
        let start = tokio::time::Instant::now();
        let (status, _, body) = send(&f.state, get("/preview/diag.png?device=kitchen", &[])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(png_depth(&body), png::BitDepth::Two);
        assert!(start.elapsed() <= limit, "{:?}", start.elapsed());
        drop(release);
    }

    #[tokio::test]
    async fn diag_preview_with_history_disabled() {
        let f = fixture();
        let (status, _, body) = send(&f.state, get("/preview/diag.png?device=office", &[])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(png_depth(&body), png::BitDepth::Two);
    }

    #[tokio::test]
    async fn diag_preview_does_not_touch_image_store() {
        let f = fixture();
        let (status, _, _) = send(
            &f.state,
            get("/api/display", &[("Access-Token", "kitchen-key")]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let before = f.state.images.latest("kitchen");
        assert!(before.is_some());
        let (status, _, _) = send(&f.state, get("/preview/diag.png?device=kitchen", &[])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(f.state.images.latest("kitchen"), before);
    }

    #[tokio::test]
    async fn display_records_a_reading() {
        let dir = tempfile::tempdir().unwrap();
        let history = open_history(dir.path(), warsaw(7, 0));
        let f = fixture_with(7, 0, history.clone(), dir.path().to_owned());
        let (status, _) = send_json(
            &f.state,
            get(
                "/api/display",
                &[
                    ("Access-Token", "kitchen-key"),
                    ("Battery-Voltage", "4.01"),
                    ("RSSI", "-84"),
                    ("Wake-Time", "4123"),
                    ("Reset-Reason", "ESP_RST_DEEPSLEEP"),
                    ("Update-Source", "timer"),
                    ("Image-Cached", "true"),
                    ("Refresh-Rate", "900"),
                    ("FW-Version", "1.8.19"),
                    ("Model", "xiao_epaper_display"),
                ],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        history.flush();
        let r = history
            .snapshot(KITCHEN_MAC, warsaw(7, 0))
            .unwrap()
            .latest
            .unwrap();
        assert_eq!(r.battery_v, Some(4.01));
        assert_eq!(r.rssi, Some(-84));
        assert_eq!(r.wake_ms, Some(4123));
        assert_eq!(r.reset_reason.as_deref(), Some("ESP_RST_DEEPSLEEP"));
        assert_eq!(r.update_source.as_deref(), Some("timer"));
        assert_eq!(r.image_cached, Some(true));
        assert_eq!(r.refresh_rate, Some(900));
        assert_eq!(r.fw_version.as_deref(), Some("1.8.19"));
        assert_eq!(r.model.as_deref(), Some("xiao_epaper_display"));
        assert_eq!(r.next_refresh, Some(900));
        assert!(r.render_ms.is_some());
        assert!(r.image_bytes.is_some_and(|n| n > 0));
    }

    #[tokio::test]
    async fn failed_render_without_image_records_a_bare_reading() {
        let plain = fixture();
        plain.alpha.fail_render.store(true, Ordering::SeqCst);
        let kitchen = || {
            get(
                "/api/display",
                &[("Access-Token", "kitchen-key"), ("Battery-Voltage", "3.9")],
            )
        };
        let (want_status, want_headers, want_body) = send(&plain.state, kitchen()).await;
        assert_eq!(want_status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            serde_json::from_slice::<Value>(&want_body).unwrap(),
            serde_json::json!({"status": 503, "error": "image not available", "reset_firmware": false})
        );

        let dir = tempfile::tempdir().unwrap();
        let history = open_history(dir.path(), warsaw(7, 0));
        let f = fixture_with(7, 0, history.clone(), dir.path().to_owned());
        f.alpha.fail_render.store(true, Ordering::SeqCst);
        let (status, headers, body) = send(&f.state, kitchen()).await;
        assert_eq!(status, want_status);
        assert_eq!(headers, want_headers);
        assert_eq!(body, want_body);

        history.flush();
        let r = history
            .snapshot(KITCHEN_MAC, warsaw(7, 0))
            .unwrap()
            .latest
            .unwrap();
        assert_eq!(r.battery_v, Some(3.9));
        assert_eq!(r.render_ms, None);
        assert_eq!(r.image_bytes, None);
        assert_eq!(r.next_refresh, None);
    }

    #[tokio::test]
    async fn hostile_headers_are_null_and_reply_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let history = open_history(dir.path(), warsaw(7, 0));
        let f = fixture_with(7, 0, history.clone(), dir.path().to_owned());
        let (_, _, plain) = send(
            &f.state,
            get("/api/display", &[("Access-Token", "kitchen-key")]),
        )
        .await;
        let long = "x".repeat(10 * 1024);
        for voltage in ["NaN", "inf"] {
            let (_, _, hostile) = send(
                &f.state,
                get(
                    "/api/display",
                    &[
                        ("Access-Token", "kitchen-key"),
                        ("RSSI", "abc"),
                        ("Battery-Voltage", voltage),
                        ("Wake-Time", "-5"),
                        ("Refresh-Rate", "99999999"),
                        ("FW-Version", &long),
                    ],
                ),
            )
            .await;
            assert_eq!(hostile, plain);
        }
        history.flush();
        let snap = history.snapshot(KITCHEN_MAC, warsaw(7, 0)).unwrap();
        let r = snap.latest.unwrap();
        assert_eq!(r.rssi, None);
        assert_eq!(r.battery_v, None);
        assert_eq!(r.wake_ms, None);
        assert_eq!(r.refresh_rate, None);
        assert_eq!(r.fw_version.unwrap().len(), 64);
    }

    #[tokio::test]
    async fn unknown_token_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let history = open_history(dir.path(), warsaw(7, 0));
        let f = fixture_with(7, 0, history.clone(), dir.path().to_owned());
        let (status, _, _) = send(
            &f.state,
            get(
                "/api/display",
                &[("Access-Token", "nope"), ("ID", KITCHEN_MAC)],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let log = Request::post("/api/log")
            .header("Access-Token", "nope")
            .body(Body::from("{}"))
            .unwrap();
        send(&f.state, log).await;
        history.flush();
        let snap = history.snapshot(KITCHEN_MAC, warsaw(7, 0)).unwrap();
        assert!(snap.latest.is_none() && snap.last_log.is_none());
    }

    fn post_log(token: &str, body: &str) -> Request<Body> {
        Request::post("/api/log")
            .header("Access-Token", token)
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    #[tokio::test]
    async fn device_log_stores_entries() {
        let dir = tempfile::tempdir().unwrap();
        let history = open_history(dir.path(), warsaw(7, 0));
        let f = fixture_with(7, 0, history.clone(), dir.path().to_owned());
        let body = r#"{"logs":[{"created_at":1795810000,"message":"HTTP GET failed","level":"error","wake_reason":"timer"}]}"#;
        let (status, _, _) = send(&f.state, post_log("kitchen-key", body)).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        history.flush();
        let log = history
            .snapshot(KITCHEN_MAC, warsaw(7, 0))
            .unwrap()
            .last_log
            .unwrap();
        assert_eq!(log.message.as_deref(), Some("HTTP GET failed"));
        assert!(log.created_at.is_some());

        let (status, _, _) = send(&f.state, post_log("kitchen-key", "not json")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        history.flush();
        let log = history
            .snapshot(KITCHEN_MAC, warsaw(7, 0))
            .unwrap()
            .last_log
            .unwrap();
        assert_eq!(log.raw, "not json");
    }

    #[tokio::test]
    async fn history_keeps_following_a_renamed_device() {
        let dir = tempfile::tempdir().unwrap();
        let history = open_history(dir.path(), warsaw(7, 0));
        let f = fixture_with(7, 0, history.clone(), dir.path().to_owned());
        send(
            &f.state,
            get("/api/display", &[("Access-Token", "kitchen-key")]),
        )
        .await;
        history.flush();
        history.shutdown();
        drop(f);

        let history = open_history(dir.path(), warsaw(7, 0));
        let mut f = fixture_with(7, 0, history.clone(), dir.path().to_owned());
        let mut cfg = (*f.state.cfg).clone();
        cfg.devices[0].name = "galley".into();
        f.state.cfg = Arc::new(cfg);
        send(
            &f.state,
            get("/api/display", &[("Access-Token", "kitchen-key")]),
        )
        .await;
        history.flush();
        let snap = history.snapshot(KITCHEN_MAC, warsaw(7, 0)).unwrap();
        assert_eq!(snap.readings.len(), 2);
    }

    #[tokio::test]
    async fn display_and_log_with_history_disabled() {
        let f = fixture();
        assert!(!f.state.history.is_available());
        let (status, body) = send_json(
            &f.state,
            get("/api/display", &[("Access-Token", "kitchen-key")]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["special_function"], "none");
        let (status, _, _) = send(&f.state, post_log("kitchen-key", "{}")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
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
        assert!(filename.starts_with("alpha-"), "{filename}");
        assert!(first["refresh_rate"].is_u64(), "{first}");
        assert_eq!(first["refresh_rate"], 900);
        assert_eq!(first["status"], 0);
        assert_eq!(first["reset_firmware"], false);
        assert_eq!(first["special_function"], "none");
        assert_eq!(first["update_firmware"], false);
        assert_eq!(first["firmware_url"], Value::Null);
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
        assert!(office["filename"].as_str().unwrap().starts_with("bravo-"));
        assert!(office["filename"].as_str().unwrap().len() <= 31);
        assert_eq!(office["reset_firmware"], false);
    }

    #[tokio::test]
    async fn display_uses_dashboard_switch_at() {
        let f = fixture_at(17, 50);
        let rate = |key: &'static str| {
            let state = f.state.clone();
            async move {
                let (status, body) =
                    send_json(&state, get("/api/display", &[("Access-Token", key)])).await;
                assert_eq!(status, StatusCode::OK);
                body["refresh_rate"].clone()
            }
        };
        assert_eq!(rate("kitchen-key").await, 660);
        assert_eq!(rate("office-key").await, 900);
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
        let (status, _, _) = send(&f.state, get("/images/alpha-00000000-1.png", &[])).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    fn encoded(state: &AppState) -> String {
        let mut reg = prometheus_client::registry::Registry::default();
        state.recorder.register(&mut reg);
        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &reg).unwrap();
        out
    }

    #[tokio::test]
    async fn display_records_request_and_device() {
        let f = fixture();
        let (status, _) = send_json(
            &f.state,
            get("/api/display", &[("Access-Token", "kitchen-key")]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let text = encoded(&f.state);
        assert!(
            text.contains(
                "tablica_http_requests_total{route=\"/api/display\",status=\"200\",outcome=\"served\"} 1"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "tablica_render_duration_seconds_count{dashboard=\"alpha\",kind=\"device\"} 1"
            ),
            "{text}"
        );
        let state = f.state.recorder.devices().get("kitchen").unwrap();
        assert!(state.last_seen.is_some());
    }

    #[tokio::test]
    async fn unknown_token_counts_as_refused() {
        let f = fixture();
        let (status, _) =
            send_json(&f.state, get("/api/display", &[("Access-Token", "nope")])).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let text = encoded(&f.state);
        assert!(
            text.contains(
                "tablica_http_requests_total{route=\"/api/display\",status=\"401\",outcome=\"refused\"} 1"
            ),
            "{text}"
        );
    }

    #[tokio::test]
    async fn unavailable_display_counts_as_unavailable() {
        let f = fixture();
        f.alpha.fail_render.store(true, Ordering::SeqCst);
        let (status, _) = send_json(
            &f.state,
            get("/api/display", &[("Access-Token", "kitchen-key")]),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let text = encoded(&f.state);
        assert!(
            text.contains(
                "tablica_http_requests_total{route=\"/api/display\",status=\"503\",outcome=\"unavailable\"} 1"
            ),
            "{text}"
        );
        assert!(!text.contains("outcome=\"refused\""), "{text}");
        let state = f.state.recorder.devices().get("kitchen").unwrap();
        assert!(state.last_reply.is_none());
    }

    #[tokio::test]
    async fn unmatched_route_label() {
        let f = fixture();
        let (status, _, _) = send(&f.state, get("/nope", &[])).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let text = encoded(&f.state);
        assert!(
            text.contains(
                "tablica_http_requests_total{route=\"unmatched\",status=\"404\",outcome=\"none\"} 1"
            ),
            "{text}"
        );
    }

    #[tokio::test]
    async fn main_router_has_no_metrics() {
        let f = fixture();
        let (status, _, _) = send(&f.state, get("/metrics", &[])).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn display_reuses_previous_filename_when_render_fails() {
        let f = fixture();
        let kitchen = || get("/api/display", &[("Access-Token", "kitchen-key")]);
        let (status, good) = send_json(&f.state, kitchen()).await;
        assert_eq!(status, StatusCode::OK);

        f.alpha.fail_render.store(true, Ordering::SeqCst);
        let (status, again) = send_json(&f.state, kitchen()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(again["filename"], good["filename"]);
        assert_eq!(again["reset_firmware"], false);

        // A device with no previous image gets 503.
        let fresh = fixture();
        fresh.alpha.fail_render.store(true, Ordering::SeqCst);
        let (status, body) = send_json(&fresh.state, kitchen()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["reset_firmware"], false);
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
        let morning = fetch("/preview/alpha.png?at=2026-11-25T07:00:00%2B01:00").await;
        assert_eq!(png_depth(&morning), png::BitDepth::Two);
        // An unencoded `+` arrives as a space; it still parses.
        let morning_raw = fetch("/preview/alpha.png?at=2026-11-25T07:00:00+01:00").await;
        assert_eq!(morning, morning_raw);
        // No `at` means the clock, which is pinned to that same morning.
        assert_eq!(fetch("/preview/alpha.png").await, morning);
        let evening = fetch("/preview/alpha.png?at=2026-11-25T19:00:00%2B01:00").await;
        assert_ne!(morning, evening);
        let one = fetch("/preview/alpha.png?at=2026-11-25T07:00:00%2B01:00&depth=1").await;
        assert_eq!(png_depth(&one), png::BitDepth::One);
        let bravo = fetch("/preview/bravo.png?depth=2").await;
        assert_eq!(png_depth(&bravo), png::BitDepth::Two);

        // Preview never stores images.
        assert!(f.state.images.latest("kitchen").is_none());

        for (uri, code) in [
            ("/preview/other.png", StatusCode::NOT_FOUND),
            ("/preview/alpha", StatusCode::NOT_FOUND),
            ("/preview/alpha.png?depth=3", StatusCode::BAD_REQUEST),
            ("/preview/alpha.png?at=yesterday", StatusCode::BAD_REQUEST),
        ] {
            let (status, _, _) = send(&f.state, get(uri, &[])).await;
            assert_eq!(status, code, "{uri}");
        }
    }

    #[tokio::test]
    async fn preview_query_parsing_applies_to_any_dashboard() {
        let f = fixture();
        for (uri, code) in [
            (
                "/preview/bravo.png?at=2026-11-25T07:00:00 01:00",
                StatusCode::OK,
            ),
            ("/preview/bravo.png?at=nope", StatusCode::BAD_REQUEST),
            ("/preview/bravo.png?depth=3", StatusCode::BAD_REQUEST),
            ("/preview/zulu.png", StatusCode::NOT_FOUND),
        ] {
            let uri = uri.replace(' ', "%20");
            let (status, _, _) = send(&f.state, get(&uri, &[])).await;
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

    /// Runs `f` with every log line at `debug` and above that this thread emits
    /// captured, and returns them.
    ///
    /// A scoped subscriber is not enough: other tests hit the same callsites
    /// concurrently and can cache "never interested" for it. So one global
    /// subscriber is installed for the test binary, and its writer appends to the
    /// capturing thread's buffer (the test runtime is single-threaded, so the
    /// middleware and handlers run on that thread) and drops everything else.
    async fn captured_logs<F: Future<Output = ()>>(f: F) -> String {
        use std::cell::RefCell;
        use std::sync::Once;

        thread_local! {
            static CAPTURE: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
        }
        struct ThreadSink;
        impl std::io::Write for ThreadSink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                CAPTURE.with_borrow_mut(|c| {
                    if let Some(c) = c {
                        c.extend_from_slice(buf);
                    }
                });
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .with_writer(|| ThreadSink)
                .init();
        });

        CAPTURE.set(Some(Vec::new()));
        f.await;
        let bytes = CAPTURE.take().unwrap_or_default();
        String::from_utf8(bytes).unwrap()
    }

    fn line_with<'a>(logs: &'a str, needle: &str) -> &'a str {
        logs.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line with {needle:?} in:\n{logs}"))
    }

    /// `req` as if it arrived over TCP from `peer`.
    fn from_peer(mut req: Request<Body>, peer: &str) -> Request<Body> {
        let addr = std::net::SocketAddr::new(peer.parse().unwrap(), 40000);
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(addr));
        req
    }

    #[tokio::test]
    async fn access_log_records_each_request() {
        let f = fixture();
        let logs = captured_logs(async {
            let display = get(
                "/api/display",
                &[
                    ("Access-Token", "kitchen-key"),
                    ("ID", "aa-bb-cc-dd-ee-01"),
                    ("RSSI", "-69"),
                    ("Battery-Voltage", "4.1"),
                    ("FW-Version", "1.6.2"),
                    // The proxy appended the last hop; the first came from the caller.
                    ("X-Forwarded-For", "6.6.6.6, 203.0.113.7"),
                ],
            );
            let (_, body) = send_json(&f.state, from_peer(display, "127.0.0.1")).await;
            let path = body["image_url"]
                .as_str()
                .unwrap()
                .strip_prefix(BASE)
                .unwrap()
                .to_owned();
            send(&f.state, get(&path, &[])).await;
            send(&f.state, get("/healthz", &[])).await;
        })
        .await;

        let display = line_with(&logs, "GET /api/display 200");
        for field in [
            "tablica::access",
            " INFO ",
            r#"device="kitchen""#,
            r#"mac="AA:BB:CC:DD:EE:01""#,
            "client=203.0.113.7",
            "peer=127.0.0.1",
            r#"rssi="-69""#,
            r#"battery_voltage="4.1""#,
            r#"fw_version="1.6.2""#,
            &format!(r#"image_url="{BASE}/images/alpha-"#),
        ] {
            assert!(display.contains(field), "{field} missing: {display}");
        }
        assert!(!display.contains("reason="), "{display}");

        let image = line_with(&logs, "GET /images/alpha-");
        assert!(image.contains("bytes="), "{image}");
        assert!(line_with(&logs, "GET /healthz 200").contains("DEBUG"));
    }

    #[tokio::test]
    async fn access_log_trusts_forwarded_for_only_from_the_proxy() {
        let f = fixture();
        let cases = [
            // Direct caller: its own header is ignored.
            ("192.168.1.50", Some("1.2.3.4"), "client=192.168.1.50"),
            // Proxy with an unparseable hop: fall back to the peer.
            ("127.0.0.1", Some("1.2.3.4, garbage"), "client=127.0.0.1"),
            ("127.0.0.1", None, "client=127.0.0.1"),
            ("::1", Some("2001:db8::7"), "client=2001:db8::7"),
        ];
        for (peer, forwarded, expected) in cases {
            let logs = captured_logs(async {
                let headers: Vec<_> = forwarded
                    .map(|v| ("X-Forwarded-For", v))
                    .into_iter()
                    .collect();
                send(
                    &f.state,
                    from_peer(get("/preview/alpha.png", &headers), peer),
                )
                .await;
            })
            .await;
            let line = line_with(&logs, "GET /preview/alpha.png 200");
            assert!(line.contains(expected), "{peer} {forwarded:?}: {line}");
            assert!(line.contains(&format!("peer={peer}")), "{line}");
        }
    }

    #[tokio::test]
    async fn access_log_never_records_credentials() {
        let f = fixture();
        let logs = captured_logs(async {
            send(
                &f.state,
                get("/api/display", &[("Access-Token", "kitchen-key")]),
            )
            .await;
            send(
                &f.state,
                get("/preview/alpha.png?api_key=sentinel-query&depth=2", &[]),
            )
            .await;
            send(
                &f.state,
                get(
                    "/api/display",
                    &[("Access-Token", "sentinel-token"), ("ID", "sentinel-id")],
                ),
            )
            .await;
            send(&f.state, get("/api/setup", &[("ID", "sentinel-setup-id")])).await;
        })
        .await;
        for secret in ["kitchen-key", "sentinel"] {
            assert!(!logs.contains(secret), "{secret} leaked:\n{logs}");
        }
        // The path is logged without its query.
        line_with(&logs, "GET /preview/alpha.png 200");
        // A malformed ID is dropped, not logged as a MAC.
        assert!(!line_with(&logs, "GET /api/display 401").contains("mac="));
        assert!(!line_with(&logs, "GET /api/setup 404").contains("mac="));
    }

    #[tokio::test]
    async fn access_log_names_the_device_the_handler_served() {
        let f = fixture();
        let logs = captured_logs(async {
            // A re-provisioned device: stale kitchen key, office MAC. Setup serves office.
            let (_, body) = send_json(
                &f.state,
                get(
                    "/api/setup",
                    &[("Access-Token", "kitchen-key"), ("ID", "AA:BB:CC:DD:EE:02")],
                ),
            )
            .await;
            assert_eq!(body["api_key"], "office-key");
        })
        .await;
        let setup = line_with(&logs, "GET /api/setup 200");
        assert!(setup.contains(r#"device="office""#), "{setup}");
        assert!(
            setup.contains(&format!(r#"image_url="{BASE}/preview/bravo.png?depth=1""#)),
            "{setup}"
        );
    }

    #[tokio::test]
    async fn access_log_has_one_warn_per_refused_request() {
        let f = fixture();
        let logs = captured_logs(async {
            send(
                &f.state,
                get(
                    "/api/display",
                    &[("Access-Token", "wrong"), ("ID", "AA:BB:CC:DD:EE:01")],
                ),
            )
            .await;
            send(&f.state, get("/api/display", &[("Access-Token", "wrong")])).await;
            send(&f.state, get("/api/setup", &[("ID", "AA:BB:CC:DD:EE:99")])).await;
            let log = Request::post("/api/log")
                .header("Access-Token", "wrong")
                .body(Body::from("{}"))
                .unwrap();
            send(&f.state, log).await;
        })
        .await;
        let warns: Vec<_> = logs.lines().filter(|l| l.contains(" WARN ")).collect();
        assert_eq!(warns.len(), 4, "{logs}");
        assert!(
            warns.iter().all(|l| l.contains("tablica::access")),
            "{logs}"
        );

        let stale = line_with(&logs, r#"mac="AA:BB:CC:DD:EE:01""#);
        assert!(
            stale.contains(r#"reason="unknown access token; ID is configured device kitchen""#),
            "{stale}"
        );
        assert!(!stale.contains("device="), "{stale}");
        let setup = line_with(&logs, "GET /api/setup 404");
        assert!(setup.contains(r#"reason="unknown device""#), "{setup}");
        let log = line_with(&logs, "POST /api/log 401");
        assert!(log.contains(r#"reason="unknown access token""#), "{log}");
    }

    #[tokio::test]
    async fn healthz_lists_each_active_dashboard() {
        let f = fixture();
        let (status, body) = send_json(&f.state, get("/healthz", &[])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({
                "status": "ok",
                "dashboards": {
                    "alpha": {"stub": "alpha"},
                    "bravo": {"stub": "bravo"},
                },
            })
        );
    }
}
