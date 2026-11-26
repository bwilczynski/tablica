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
