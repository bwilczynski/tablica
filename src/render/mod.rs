//! Template rendering and PNG output: MiniJinja -> Takumi -> quantize -> PNG.

mod quantize;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::Context;
use image::RgbaImage;
use minijinja::Environment;
use minijinja_autoreload::AutoReloader;
use serde::Serialize;
use takumi::prelude::*;
use tracing::error;

use crate::config::BitDepth;
pub use quantize::{encode_png, quantize};

/// Largest PNG the device is expected to accept.
pub const MAX_PNG_BYTES: usize = 90_000;

const WIDTH: u32 = 800;
const HEIGHT: u32 = 480;
const INTER: &[u8] = include_bytes!("../../assets/fonts/Inter.ttf");

/// Where templates come from.
#[derive(Debug, Clone)]
pub enum TemplateSource {
    /// Templates compiled into the binary.
    Embedded,
    /// Templates read from disk, reloaded when the files change.
    Dir(PathBuf),
}

enum Templates {
    Embedded(Environment<'static>),
    Dir(Box<AutoReloader>),
}

/// Newest modification time and total size of all files under `dir`.
fn dir_signature(dir: &Path) -> (Option<SystemTime>, u64) {
    let mut sig = (None, 0);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                sig.0 = sig.0.max(meta.modified().ok());
                sig.1 += meta.len();
            }
        }
    }
    sig
}

pub struct Renderer {
    templates: Templates,
    fonts: Fonts,
}

impl Renderer {
    pub fn new(src: TemplateSource) -> anyhow::Result<Renderer> {
        let templates = match src {
            TemplateSource::Embedded => {
                let mut env = Environment::new();
                minijinja_embed::load_templates!(env);
                Templates::Embedded(env)
            }
            TemplateSource::Dir(dir) => {
                Templates::Dir(Box::new(AutoReloader::new(move |notifier| {
                    let mut env = Environment::new();
                    env.set_loader(minijinja::path_loader(&dir));
                    notifier.set_fast_reload(true);
                    // Poll a cheap directory signature instead of the `watch-fs`
                    // feature: no extra dependencies, and deterministic.
                    let dir = dir.clone();
                    let last = Mutex::new(dir_signature(&dir));
                    notifier.set_callback(move || {
                        let now = dir_signature(&dir);
                        let mut last = last.lock().unwrap();
                        std::mem::replace(&mut *last, now) != *last
                    });
                    Ok(env)
                })))
            }
        };
        let mut fonts = Fonts::default();
        fonts
            .register(FontResource::new(INTER))
            .map_err(|e| anyhow::anyhow!("registering Inter: {e:?}"))?;
        Ok(Renderer { templates, fonts })
    }

    /// Renders `template` with `ctx` into an 800x480 grayscale PNG.
    pub fn render<S: Serialize>(
        &self,
        template: &str,
        ctx: &S,
        depth: BitDepth,
    ) -> anyhow::Result<Vec<u8>> {
        let html = match &self.templates {
            Templates::Embedded(env) => env.get_template(template)?.render(ctx),
            Templates::Dir(reloader) => {
                let env = reloader.acquire_env()?;
                env.get_template(template)?.render(ctx)
            }
        }
        .with_context(|| format!("rendering template {template}"))?;

        let rgba = self.rasterize(&html)?;
        let png = encode_png(&quantize(&rgba, depth), depth);
        if png.len() > MAX_PNG_BYTES {
            error!(
                template,
                bytes = png.len(),
                limit = MAX_PNG_BYTES,
                "rendered PNG exceeds the size limit"
            );
        }
        Ok(png)
    }

    fn rasterize(&self, html: &str) -> anyhow::Result<RgbaImage> {
        let parsed = takumi::parse_html(html, FromHtmlOptions::default())
            .map_err(|e| anyhow::anyhow!("parsing HTML: {e:?}"))?;
        let sheet = StyleSheet::parse_loosy(&parsed.css.join("\n"));
        let opts = RenderOptions::builder()
            .viewport(Viewport::new((WIDTH, HEIGHT)))
            .node(parsed.node)
            .fonts(&self.fonts)
            .stylesheet(sheet.into())
            .build();
        let bitmap = takumi::render(opts).map_err(|e| anyhow::anyhow!("rendering: {e:?}"))?;
        let (w, h) = (bitmap.width(), bitmap.height());
        RgbaImage::from_raw(w, h, bitmap.into_raw()).context("bitmap size mismatch")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboards::homelab;
    use crate::dashboards::test_support::{
        sample_inputs, sample_view_evening, sample_view_morning, worst_case_inputs, worst_case_view,
    };

    fn dims(png_bytes: &[u8]) -> (u32, u32, png::BitDepth) {
        let r = png::Decoder::new(std::io::Cursor::new(png_bytes))
            .read_info()
            .unwrap();
        (r.info().width, r.info().height, r.info().bit_depth)
    }

    fn renderer() -> Renderer {
        Renderer::new(TemplateSource::Embedded).unwrap()
    }

    #[test]
    fn family_render_is_valid_png() {
        let r = renderer();
        for (depth, bits) in [
            (BitDepth::Two, png::BitDepth::Two),
            (BitDepth::One, png::BitDepth::One),
        ] {
            let png = r
                .render("family.html", &sample_view_morning(), depth)
                .unwrap();
            assert_eq!(dims(&png), (800, 480, bits));
            assert!(png.len() <= MAX_PNG_BYTES, "{} bytes", png.len());
        }
    }

    #[test]
    fn worst_case_family_render_fits_size_limit() {
        let r = renderer();
        for depth in [BitDepth::Two, BitDepth::One] {
            let png = r.render("family.html", &worst_case_view(), depth).unwrap();
            assert_eq!((dims(&png).0, dims(&png).1), (800, 480));
            assert!(png.len() <= MAX_PNG_BYTES, "{} bytes", png.len());
        }
    }

    #[test]
    fn homelab_render_is_valid_png() {
        let png = renderer()
            .render("homelab.html", &homelab::build(), BitDepth::Two)
            .unwrap();
        assert_eq!(dims(&png), (800, 480, png::BitDepth::Two));
    }

    #[test]
    fn embedded_and_dir_templates_match() {
        let dir = Renderer::new(TemplateSource::Dir(PathBuf::from("templates"))).unwrap();
        let view = sample_view_morning();
        let a = renderer()
            .render("family.html", &view, BitDepth::Two)
            .unwrap();
        let b = dir.render("family.html", &view, BitDepth::Two).unwrap();
        assert_eq!(a, b);
    }

    /// Writes preview PNGs to `target/preview/` for eyeballing against the mockup.
    #[test]
    fn write_previews() {
        let r = renderer();
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/preview");
        std::fs::create_dir_all(&out).unwrap();
        let jobs = [
            ("family-morning-2bit", sample_view_morning(), BitDepth::Two),
            ("family-evening-2bit", sample_view_evening(), BitDepth::Two),
            ("family-morning-1bit", sample_view_morning(), BitDepth::One),
            ("family-worst-2bit", worst_case_view(), BitDepth::Two),
        ];
        for (name, view, depth) in jobs {
            let png = r.render("family.html", &view, depth).unwrap();
            std::fs::write(out.join(format!("{name}.png")), png).unwrap();
        }
        let _ = (sample_inputs, worst_case_inputs);
    }

    #[test]
    fn dir_templates_reload_after_edit() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::copy("templates/homelab.html", tmp.path().join("homelab.html")).unwrap();
        let r = Renderer::new(TemplateSource::Dir(tmp.path().to_path_buf())).unwrap();
        let ctx = homelab::build();
        let before = r.render("homelab.html", &ctx, BitDepth::Two).unwrap();
        let edited = std::fs::read_to_string(tmp.path().join("homelab.html"))
            .unwrap()
            .replace("font-size: 48px", "font-size: 24px");
        std::fs::write(tmp.path().join("homelab.html"), edited).unwrap();
        let after = r.render("homelab.html", &ctx, BitDepth::Two).unwrap();
        assert_ne!(before, after);
    }
}
