//! Renders the real family and homelab templates, embedded and from a directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use crate::config::{BitDepth, Config, Device, Schedule};
use crate::dashboard::{Active, Dashboard, ParsedDashboard, PrepareContext, View};
use crate::dashboards::family::FamilyView;
use crate::dashboards::homelab::{self, HomelabDashboard};
use crate::dashboards::test_support::{
    sample_inputs, sample_view_evening, sample_view_morning, time, warsaw, worst_case_view,
};
use crate::device_api::{AppState, ImageStore, router};
use crate::family::dashboard::{Endpoints, ParsedFamily};
use crate::render::{MAX_PNG_BYTES, Renderer, TemplateSource};

fn templates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("templates")
}

/// Family (caches seeded with the sample at 07:00 Warsaw) and homelab, as served.
fn active(data_dir: &Path) -> Active {
    let sample = sample_inputs(warsaw(2026, 11, 25, 7, 0));
    sample.seed(data_dir);
    let ctx = PrepareContext {
        timezone: chrono_tz::Europe::Warsaw,
        data_dir,
    };
    let family = Box::new(ParsedFamily {
        cfg: sample.cfg,
        endpoints: Endpoints::default(),
    })
    .prepare(&ctx)
    .unwrap();
    let homelab: Arc<dyn Dashboard> = Arc::new(HomelabDashboard);
    Active::from([("family", family), ("homelab", homelab)])
}

fn renderer(src: TemplateSource) -> Renderer {
    let dir = tempfile::tempdir().unwrap();
    Renderer::new(src, &active(dir.path())).unwrap()
}

fn family_view(view: FamilyView) -> View {
    View {
        template: "family.html",
        context: minijinja::Value::from_serialize(view),
    }
}

fn homelab_view() -> View {
    View {
        template: "homelab.html",
        context: minijinja::Value::from_serialize(homelab::build()),
    }
}

fn dims(png_bytes: &[u8]) -> (u32, u32, png::BitDepth) {
    let r = png::Decoder::new(std::io::Cursor::new(png_bytes))
        .read_info()
        .unwrap();
    (r.info().width, r.info().height, r.info().bit_depth)
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

#[test]
fn family_render_is_valid_png() {
    let r = renderer(TemplateSource::Embedded);
    for (depth, bits) in [
        (BitDepth::Two, png::BitDepth::Two),
        (BitDepth::One, png::BitDepth::One),
    ] {
        let png = r
            .render("family", &family_view(sample_view_morning()), depth)
            .unwrap();
        assert_eq!(dims(&png), (800, 480, bits));
        assert!(png.len() <= MAX_PNG_BYTES, "{} bytes", png.len());
    }
}

#[test]
fn worst_case_family_render_fits_size_limit() {
    let r = renderer(TemplateSource::Embedded);
    for depth in [BitDepth::Two, BitDepth::One] {
        let png = r
            .render("family", &family_view(worst_case_view()), depth)
            .unwrap();
        assert_eq!((dims(&png).0, dims(&png).1), (800, 480));
        assert!(png.len() <= MAX_PNG_BYTES, "{} bytes", png.len());
    }
}

#[test]
fn homelab_render_is_valid_png() {
    let png = renderer(TemplateSource::Embedded)
        .render("homelab", &homelab_view(), BitDepth::Two)
        .unwrap();
    assert_eq!(dims(&png), (800, 480, png::BitDepth::Two));
}

#[test]
fn embedded_and_dir_templates_match() {
    let embedded = renderer(TemplateSource::Embedded);
    let dir = renderer(TemplateSource::Dir(templates_dir()));
    let view = family_view(sample_view_morning());
    let a = embedded.render("family", &view, BitDepth::Two).unwrap();
    let b = dir.render("family", &view, BitDepth::Two).unwrap();
    assert_eq!(a, b);
    let a = embedded
        .render("homelab", &homelab_view(), BitDepth::Two)
        .unwrap();
    let b = dir
        .render("homelab", &homelab_view(), BitDepth::Two)
        .unwrap();
    assert_eq!(a, b);
}

/// Writes preview PNGs to `<workspace root>/target/preview/` for eyeballing against the mockup.
#[test]
fn write_previews() {
    let r = renderer(TemplateSource::Embedded);
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/preview");
    std::fs::create_dir_all(&out).unwrap();
    let jobs = [
        ("family-morning-2bit", sample_view_morning(), BitDepth::Two),
        ("family-evening-2bit", sample_view_evening(), BitDepth::Two),
        ("family-morning-1bit", sample_view_morning(), BitDepth::One),
        ("family-worst-2bit", worst_case_view(), BitDepth::Two),
    ];
    for (name, view, depth) in jobs {
        let png = r.render("family", &family_view(view), depth).unwrap();
        std::fs::write(out.join(format!("{name}.png")), png).unwrap();
    }
}

#[test]
fn dir_templates_reload_after_edit() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::copy(
        templates_dir().join("homelab.html"),
        tmp.path().join("homelab.html"),
    )
    .unwrap();
    let r = renderer(TemplateSource::Dir(tmp.path().to_path_buf()));
    let before = r.render("homelab", &homelab_view(), BitDepth::Two).unwrap();
    let edited = std::fs::read_to_string(tmp.path().join("homelab.html"))
        .unwrap()
        .replace("font-size: 48px", "font-size: 24px");
    std::fs::write(tmp.path().join("homelab.html"), edited).unwrap();
    let after = r.render("homelab", &homelab_view(), BitDepth::Two).unwrap();
    assert_ne!(before, after);
}

#[test]
fn dir_reload_after_editing_included_svg() {
    let tmp = tempfile::tempdir().unwrap();
    copy_dir(&templates_dir(), tmp.path());
    let r = renderer(TemplateSource::Dir(tmp.path().to_path_buf()));
    let view = family_view(sample_view_morning());
    // The sample's current weather is cloudy, so the header includes this icon.
    let icon = tmp.path().join("icons/cloudy.svg");
    assert!(icon.exists());
    let before = r.render("family", &view, BitDepth::Two).unwrap();
    std::fs::write(
        &icon,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect x="0" y="0" width="24" height="24" fill="black"/></svg>"#,
    )
    .unwrap();
    let after = r.render("family", &view, BitDepth::Two).unwrap();
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
    std::fs::copy(
        templates_dir().join("family.html"),
        templates.path().join("family.html"),
    )
    .unwrap();
    copy_dir(
        &templates_dir().join("icons"),
        &templates.path().join("icons"),
    );
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
