//! The real wiring end to end: `config.sample.toml` loaded with the real family and
//! homelab entries, prepared by core, and driven through the device API router.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::Value;
use tablica_core::config::load;
use tablica_core::dashboard::DashboardEntry;
use tablica_core::device_api::{AppState, router};
use tablica_core::render::TemplateSource;
use tablica_core::server::prepare_state;
use tablica_family::test_support::{sample_inputs, warsaw};
use tower::ServiceExt;

const SAMPLE: &str = include_str!("../../../config.sample.toml");

fn entries() -> Vec<DashboardEntry> {
    vec![tablica_family::entry(), tablica_homelab::entry()]
}

/// The sample config with secret files and a data dir under `root`; the office
/// device is switched to 1-bit. The family caches are seeded with the sample data.
fn write_setup(root: &Path) -> std::path::PathBuf {
    let secrets = root.join("secrets");
    fs::create_dir(&secrets).unwrap();
    for (name, value) in [
        ("trmnl_kitchen_key", "kitchen-key\n"),
        ("trmnl_office_key", "office-key\n"),
        ("icloud_ics_url", "webcal://example.com/cal.ics\n"),
        ("librus_kid1_login", "kid1\n"),
        ("librus_kid1_password", "pw1\n"),
        ("librus_kid2_login", "kid2\n"),
        ("librus_kid2_password", "pw2\n"),
    ] {
        fs::write(secrets.join(name), value).unwrap();
    }
    let data = root.join("data");
    fs::create_dir(&data).unwrap();
    sample_inputs(warsaw(2026, 11, 25, 7, 0)).seed(&data);

    let office = "dashboard = \"homelab\"";
    assert!(SAMPLE.contains(office));
    let text = format!(
        "data_dir = {:?}\n{}",
        data.display().to_string(),
        SAMPLE.replace(office, &format!("{office}\nbit_depth = 1"))
    );
    let config = root.join("config.toml");
    fs::write(&config, text).unwrap();
    config
}

/// The served state over the real dashboards, its clock pinned to the given Warsaw time.
fn app_at(root: &Path, h: u32, min: u32) -> AppState {
    let config = root.join("config.toml");
    assert!(config.exists());
    let loaded = load(&config, &root.join("secrets"), &entries()).unwrap();
    let fixed = warsaw(2026, 11, 25, h, min).with_timezone(&Utc);
    let (state, dashboards) =
        prepare_state(loaded, TemplateSource::Embedded, Arc::new(move || fixed)).unwrap();
    assert_eq!(dashboards.len(), 2);
    state
}

async fn get(state: &AppState, uri: &str, token: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut req = Request::get(uri);
    if let Some(token) = token {
        req = req.header("Access-Token", token);
    }
    let resp = router(state.clone())
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, body.to_vec())
}

async fn json(state: &AppState, uri: &str, token: Option<&str>) -> Value {
    let (status, body) = get(state, uri, token).await;
    assert_eq!(status, StatusCode::OK, "{uri}");
    serde_json::from_slice(&body).unwrap()
}

/// Calls `/api/display` as the device and returns the reply and the served PNG's bit depth.
async fn display(state: &AppState, token: &str) -> (Value, png::BitDepth) {
    let reply = json(state, "/api/display", Some(token)).await;
    let url = reply["image_url"].as_str().unwrap();
    let path = url.strip_prefix("http://nas.local:8080").unwrap();
    let (status, bytes) = get(state, path, None).await;
    assert_eq!(status, StatusCode::OK, "{path}");
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let reader = decoder.read_info().unwrap();
    (reply, reader.info().bit_depth)
}

#[tokio::test]
async fn end_to_end_family_and_homelab() {
    let root = tempfile::tempdir().unwrap();
    write_setup(root.path());

    let morning = app_at(root.path(), 7, 0);
    let (_, kitchen_depth) = display(&morning, "kitchen-key").await;
    assert_eq!(kitchen_depth, png::BitDepth::Two);
    let (_, office_depth) = display(&morning, "office-key").await;
    assert_eq!(office_depth, png::BitDepth::One);

    for preview in ["/preview/family.png", "/preview/homelab.png"] {
        let (status, _) = get(&morning, preview, None).await;
        assert_eq!(status, StatusCode::OK, "{preview}");
    }

    let health = json(&morning, "/healthz", None).await;
    assert_eq!(health["status"], "ok");
    assert_eq!(
        health["dashboards"]["family"]["timetables"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // Ten minutes before the family dashboard's 16:00 switch: kitchen wakes just
    // after it (10 minutes + 60 s), office keeps the regular 15-minute cadence.
    let afternoon = app_at(root.path(), 15, 50);
    let (kitchen, _) = display(&afternoon, "kitchen-key").await;
    assert_eq!(kitchen["refresh_rate"], 660);
    let (office, _) = display(&afternoon, "office-key").await;
    assert_eq!(office["refresh_rate"], 900);
}

#[tokio::test]
async fn serves_with_unusable_history() {
    let root = tempfile::tempdir().unwrap();
    write_setup(root.path());
    // A directory where the database file belongs: the history cannot open.
    fs::create_dir(root.path().join("data").join("history.db")).unwrap();

    let state = app_at(root.path(), 7, 0);
    assert!(!state.history.is_available());
    let (status, _) = get(&state, "/api/display", Some("kitchen-key")).await;
    assert_eq!(status, StatusCode::OK);
    let log = Request::post("/api/log")
        .header("Access-Token", "kitchen-key")
        .body(Body::from("{}"))
        .unwrap();
    let resp = router(state.clone()).oneshot(log).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn diagnostics_button_returns_to_dashboard_on_double_click_or_timer() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_setup(dir.path());
    let text = fs::read_to_string(&config).unwrap();
    fs::write(
        config,
        text.replace(
            "dashboard = \"homelab\"",
            "dashboard = \"homelab\"\ndiagnostics_button = true",
        ),
    )
    .unwrap();
    let state = app_at(dir.path(), 23, 0);
    let app = router(state.clone());
    let display = |special| {
        let app = app.clone();
        async move {
            let mut req = Request::get("/api/display")
                .header("Access-Token", "office-key")
                .header("Battery-Voltage", "4.10");
            if special {
                req = req.header("special_function", "true");
            }
            let res = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            serde_json::from_slice::<Value>(&res.into_body().collect().await.unwrap().to_bytes())
                .unwrap()
        }
    };
    let fetch_image = |reply: &Value| {
        let req = Request::get(format!(
            "/images/{}.png",
            reply["filename"].as_str().unwrap()
        ))
        .body(Body::empty())
        .unwrap();
        let app = app.clone();
        async move {
            let res = app.oneshot(req).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            res.into_body().collect().await.unwrap().to_bytes()
        }
    };
    let main = display(false).await;
    assert_eq!(main["special_function"], "guest_mode");
    for double_click_to_return in [true, false, true] {
        let battery = display(true).await;
        assert_eq!(battery["action"], "guest_mode");
        assert_eq!(battery["refresh_rate"], 120);
        assert_ne!(battery["filename"], main["filename"]);
        // An HTTP retry before the first image download still requests diagnostics.
        let retry = display(true).await;
        assert_eq!(retry["refresh_rate"], 120);
        let bytes = fetch_image(&retry).await;
        assert_eq!(fetch_image(&retry).await, bytes);
        let png = png::Decoder::new(std::io::Cursor::new(bytes))
            .read_info()
            .unwrap();
        assert_eq!(png.info().bit_depth, png::BitDepth::One);
        let back = display(double_click_to_return).await;
        assert_eq!(back["filename"], main["filename"]);
        assert_eq!(back["refresh_rate"], main["refresh_rate"]);
        assert_eq!(back["maximum_compatibility"], true);
        if double_click_to_return {
            assert_eq!(back["action"], "guest_mode");
            fetch_image(&back).await;
        }
    }
}
