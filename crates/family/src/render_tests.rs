//! Renders the real family templates, embedded and from a directory.

use std::path::{Path, PathBuf};

use crate::test_support::{
    sample_view_evening, sample_view_morning, seeded_family, worst_case_view,
};
use crate::view::FamilyView;
use tablica_core::config::BitDepth;
use tablica_core::dashboard::{Active, View};
use tablica_core::render::{MAX_PNG_BYTES, Renderer, TemplateSource};

fn templates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("templates")
}

fn renderer(src: TemplateSource) -> Renderer {
    let dir = tempfile::tempdir().unwrap();
    Renderer::new(src, &Active::from([("family", seeded_family(dir.path()))])).unwrap()
}

fn family_view(view: FamilyView) -> View {
    View {
        template: "family.html",
        context: minijinja::Value::from_serialize(view),
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
fn embedded_and_dir_templates_match() {
    let embedded = renderer(TemplateSource::Embedded);
    let dir = renderer(TemplateSource::Dir(templates_dir()));
    let view = family_view(sample_view_morning());
    let a = embedded.render("family", &view, BitDepth::Two).unwrap();
    let b = dir.render("family", &view, BitDepth::Two).unwrap();
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
