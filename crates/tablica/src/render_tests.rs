//! Renders the real family and homelab bundles together, and serves them through the
//! device API.

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::Value;
use tablica_core::config::{BitDepth, Config, Device, Schedule};
use tablica_core::dashboard::{Active, Dashboard, View};
use tablica_core::device_api::{AppState, ImageStore, router};
use tablica_core::history::History;
use tablica_core::render::{Renderer, TemplateSource};
use tablica_family::test_support::{sample_view_morning, seeded_family, time, warsaw};
use tablica_homelab::HomelabDashboard;
use tower::ServiceExt;

/// Family (caches seeded with the sample at 07:00 Warsaw) and homelab, as served.
fn active(data_dir: &Path) -> Active {
    let homelab: Arc<dyn Dashboard> = Arc::new(HomelabDashboard);
    Active::from([("family", seeded_family(data_dir)), ("homelab", homelab)])
}

fn renderer(src: TemplateSource) -> Renderer {
    let dir = tempfile::tempdir().unwrap();
    Renderer::new(src, &active(dir.path())).unwrap()
}

fn family_view() -> View {
    View {
        template: "family.html",
        context: minijinja::Value::from_serialize(sample_view_morning()),
    }
}

fn homelab_view() -> View {
    HomelabDashboard.view(warsaw(2026, 11, 25, 7, 0)).unwrap()
}

fn crate_dir(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(name)
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

/// The flat `--templates` layout: family's and homelab's bundles side by side
/// (`family.html`, `homelab.html`, `icons/`), copied into a fresh directory.
fn flat_templates() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    copy_dir(&crate_dir("family/templates"), tmp.path());
    copy_dir(&crate_dir("homelab/templates"), tmp.path());
    tmp
}

#[test]
fn embedded_and_dir_templates_match() {
    let embedded = renderer(TemplateSource::Embedded);
    let flat = flat_templates();
    let dir = renderer(TemplateSource::Dir(flat.path().to_owned()));
    for (name, view) in [("family", family_view()), ("homelab", homelab_view())] {
        let a = embedded.render(name, &view, BitDepth::Two).unwrap();
        let b = dir.render(name, &view, BitDepth::Two).unwrap();
        assert_eq!(a, b, "{name}");
    }
}

#[test]
fn dir_templates_reload_after_edit() {
    let tmp = tempfile::tempdir().unwrap();
    copy_dir(&crate_dir("homelab/templates"), tmp.path());
    let r = renderer(TemplateSource::Dir(tmp.path().to_path_buf()));
    let before = r.render("homelab", &homelab_view(), BitDepth::Two).unwrap();
    let edited = std::fs::read_to_string(tmp.path().join("homelab.html"))
        .unwrap()
        .replace("font-size: 48px", "font-size: 24px");
    std::fs::write(tmp.path().join("homelab.html"), edited).unwrap();
    let after = r.render("homelab", &homelab_view(), BitDepth::Two).unwrap();
    assert_ne!(before, after);
}

const BASE: &str = "http://tablica.test:8080";

fn device(name: &str, key: &str, dashboard: &str) -> Device {
    Device {
        name: name.into(),
        mac: format!("AA:BB:CC:DD:EE:{:02X}", name.len()),
        api_key: key.into(),
        dashboard: dashboard.into(),
        bit_depth: BitDepth::Two,
    }
}

/// The served app over real dashboards, clock pinned to 07:00 Warsaw.
fn state(data_dir: &Path, templates: TemplateSource) -> AppState {
    let dashboards = active(data_dir);
    let cfg = Config {
        timezone: chrono_tz::Europe::Warsaw,
        base_url: BASE.into(),
        listen: "127.0.0.1:0".parse().unwrap(),
        metrics_listen: None,
        data_dir: data_dir.to_owned(),
        schedule: Schedule {
            wake: time(6, 0),
            sleep: time(22, 0),
            refresh_minutes: 15,
        },
        devices: vec![
            device("kitchen", "kitchen-key", "family"),
            device("office", "office-key", "homelab"),
        ],
    };
    let renderer = Renderer::new(templates, &dashboards).unwrap();
    let fixed = warsaw(2026, 11, 25, 7, 0).with_timezone(&Utc);
    AppState {
        cfg: Arc::new(cfg),
        dashboards: Arc::new(dashboards),
        renderer: Arc::new(renderer),
        images: Arc::new(ImageStore::new()),
        clock: Arc::new(move || fixed),
        history: Arc::new(History::disabled()),
        diag_reads: Arc::new(tokio::sync::Semaphore::new(1)),
    }
}

async fn send(state: &AppState, uri: &str, token: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut req = Request::get(uri);
    if let Some(token) = token {
        req = req.header("Access-Token", token);
    }
    let resp = router(state.clone())
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, body)
}

#[tokio::test]
async fn family_only_override_dir() {
    let templates = tempfile::tempdir().unwrap();
    copy_dir(&crate_dir("family/templates"), templates.path());
    let src = || TemplateSource::Dir(templates.path().to_owned());

    let data = tempfile::tempdir().unwrap();
    let s = state(data.path(), src());
    assert_eq!(
        send(&s, "/preview/family.png", None).await.0,
        StatusCode::OK
    );
    assert_eq!(
        send(&s, "/preview/homelab.png", None).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let (status, _) = send(&s, "/api/display", Some("kitchen-key")).await;
    assert_eq!(status, StatusCode::OK);

    // A homelab device falls back to its previous image...
    let earlier = (s.clock)() - chrono::Duration::minutes(15);
    let previous = s.images.put("office", "homelab", vec![1, 2, 3], earlier);
    let (status, body) = send(&s, "/api/display", Some("office-key")).await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["filename"], previous.as_str());

    // ...and gets 503 when it has none.
    let fresh = state(data.path(), src());
    let (status, _) = send(&fresh, "/api/display", Some("office-key")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn preview_family_at_switch() {
    let data = tempfile::tempdir().unwrap();
    let s = state(data.path(), TemplateSource::Embedded);
    let (status, morning) = send(
        &s,
        "/preview/family.png?at=2026-11-25T07:00:00%2B01:00",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, evening) = send(
        &s,
        "/preview/family.png?at=2026-11-25T19:00:00%2B01:00",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(morning, evening);
}

fn diag_view() -> View {
    use tablica_core::diagnostics::view::{DiagInput, build};
    let device = device("kitchen", "kitchen-key", "family");
    build(DiagInput {
        device: &device,
        history: Ok(Default::default()),
        sections: Vec::new(),
        at: warsaw(2026, 11, 25, 7, 0).with_timezone(&Utc),
        at_given: false,
        tz: chrono_tz::Europe::Warsaw,
    })
}

#[test]
fn diag_falls_back_to_embedded_in_dir_mode() {
    let embedded = renderer(TemplateSource::Embedded)
        .render_diag(&diag_view(), BitDepth::Two)
        .unwrap();
    let flat = flat_templates();
    let dir = renderer(TemplateSource::Dir(flat.path().to_owned()));
    assert_eq!(
        dir.render_diag(&diag_view(), BitDepth::Two).unwrap(),
        embedded
    );

    std::fs::write(
        flat.path().join("diag.html"),
        r#"<div style="width: 800px; height: 480px; background: black;"></div>"#,
    )
    .unwrap();
    assert_ne!(
        dir.render_diag(&diag_view(), BitDepth::Two).unwrap(),
        embedded
    );
}

#[tokio::test]
async fn diag_preview_for_family_and_homelab() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), TemplateSource::Embedded);
    for device in ["kitchen", "office"] {
        let (status, bytes) =
            send(&state, &format!("/preview/diag.png?device={device}"), None).await;
        assert_eq!(status, StatusCode::OK, "{device}");
        let reader = png::Decoder::new(std::io::Cursor::new(bytes))
            .read_info()
            .unwrap();
        assert_eq!((reader.info().width, reader.info().height), (800, 480));
    }
}

fn diag_at() -> chrono::DateTime<Utc> {
    warsaw(2026, 11, 25, 7, 0).with_timezone(&Utc)
}

/// A battery device: four days of steady drain every 15 min, a brownout two days
/// ago, one error log, last check-in 12 minutes before `diag_at()`.
fn battery_history(mac: &str) -> tablica_core::history::DeviceHistory {
    use chrono::Duration;
    use tablica_core::history::{DeviceHistory, DeviceLog, Reading};
    let latest_at = diag_at() - Duration::minutes(12);
    let readings: Vec<Reading> = (0..=4 * 96)
        .rev()
        .map(|i| Reading {
            device: mac.into(),
            at: latest_at - Duration::minutes(15 * i),
            battery_v: Some(4.00 - 0.10 * (4 * 96 - i) as f64 / (4.0 * 96.0)),
            rssi: Some(-84),
            wifi_band: Some("2.4 GHz".into()),
            wake_ms: Some(4100 + (i % 3) * 3900),
            reset_reason: Some("ESP_RST_DEEPSLEEP".into()),
            refresh_rate: Some(900),
            next_refresh: Some(900),
            fw_version: Some("1.8.19".into()),
            fw_commit: Some("abc1234".into()),
            model: Some("og".into()),
            ..Reading::default()
        })
        .collect();
    let latest = readings.last().cloned().unwrap();
    let brownout = Reading {
        reset_reason: Some("ESP_RST_BROWNOUT".into()),
        ..readings[readings.len() - 1 - 2 * 96].clone()
    };
    DeviceHistory {
        latest: Some(latest),
        readings,
        last_problem: Some(brownout),
        last_log: Some(DeviceLog {
            device: mac.into(),
            received_at: diag_at() - Duration::minutes(15),
            created_at: None,
            level: Some("error".into()),
            message: Some("display refresh failed: timeout".into()),
            wake_reason: None,
            raw: "{}".into(),
        }),
    }
}

/// Writes the diagnostics previews to `<workspace root>/target/preview/`.
#[test]
fn write_previews_diagnostics() {
    use tablica_core::diagnostics::view::{DiagInput, build};
    use tablica_core::diagnostics::{Row, Section, Status};
    use tablica_core::history::DeviceHistory;

    let dir = tempfile::tempdir().unwrap();
    let dashboards = active(dir.path());
    let r = Renderer::new(TemplateSource::Embedded, &dashboards).unwrap();
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/preview");
    std::fs::create_dir_all(&out).unwrap();

    let family = device("kitchen", "kitchen-key", "family");
    let homelab = device("office", "office-key", "homelab");
    let family_sections = dashboards["family"].diagnostics(diag_at());
    let input = |device, history, sections| DiagInput {
        device,
        history: Ok(history),
        sections,
        at: diag_at(),
        at_given: false,
        tz: chrono_tz::Europe::Warsaw,
    };

    let mut dense = battery_history(&family.mac);
    dense.last_log.as_mut().unwrap().message = Some("Wwwwwwwwww ".repeat(100));
    let noted = |title: &str| Section {
        title: title.into(),
        rows: (0..8)
            .map(|i| Row {
                label: format!("Timetable {i} Wwwwwww"),
                value: "47 h".into(),
                status: [Status::Ok, Status::Warn, Status::Bad, Status::Unknown][i % 4],
                note: Some("W".repeat(500)),
            })
            .collect(),
    };

    let jobs = [
        (
            "diag-battery-2bit",
            build(input(
                &family,
                battery_history(&family.mac),
                family_sections.clone(),
            )),
            BitDepth::Two,
        ),
        (
            "diag-battery-1bit",
            build(input(
                &family,
                battery_history(&family.mac),
                family_sections,
            )),
            BitDepth::One,
        ),
        (
            "diag-dense-2bit",
            build(input(
                &family,
                dense,
                vec![noted("Sources"), noted("Other")],
            )),
            BitDepth::Two,
        ),
        (
            "diag-empty-2bit",
            build(input(&homelab, DeviceHistory::default(), Vec::new())),
            BitDepth::Two,
        ),
        (
            "diag-sections-2bit",
            build(input(
                &family,
                battery_history(&family.mac),
                (0..10)
                    .map(|i| Section {
                        title: format!("Section {i}"),
                        rows: vec![Row {
                            label: format!("Source {i}"),
                            value: "5 min".into(),
                            status: Status::Ok,
                            note: None,
                        }],
                    })
                    .collect(),
            )),
            BitDepth::Two,
        ),
    ];
    for (name, view, depth) in jobs {
        let png = r.render_diag(&view, depth).unwrap();
        std::fs::write(out.join(format!("{name}.png")), png).unwrap();
    }
}
