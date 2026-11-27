//! Template rendering and PNG output: MiniJinja -> Takumi -> quantize -> PNG.

mod quantize;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::Context;
use image::RgbaImage;
use minijinja::Environment;
use minijinja_autoreload::AutoReloader;
use takumi::prelude::*;
use tracing::error;

use crate::config::BitDepth;
use crate::dashboard::{Active, Dashboard, View};
pub use quantize::{encode_png, quantize};

/// Largest PNG the device is expected to accept.
pub const MAX_PNG_BYTES: usize = 90_000;

/// The diagnostics screen's template, core's own.
pub const DIAG_TEMPLATE: &str = "diag.html";

const WIDTH: u32 = 800;
const HEIGHT: u32 = 480;
const INTER: &[u8] = include_bytes!("../../../../assets/fonts/Inter.ttf");

/// Where templates come from.
#[derive(Debug, Clone)]
pub enum TemplateSource {
    /// Templates compiled into the binary.
    Embedded,
    /// Templates read from disk, reloaded when the files change.
    Dir(PathBuf),
}

enum Templates {
    /// One environment per dashboard, filled from its embedded bundle.
    Embedded(BTreeMap<&'static str, Environment<'static>>),
    /// One flat directory shared by every dashboard.
    Dir(Box<AutoReloader>),
}

/// An environment holding `dashboard`'s embedded bundle. `load_templates!` compiles
/// every template and panics on invalid syntax, so a broken bundle fails startup.
fn embedded_env(dashboard: &dyn Dashboard) -> Environment<'static> {
    let mut env = Environment::new();
    dashboard.load_templates(&mut env);
    env
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

/// Core's own environment, named `diag`, holding the embedded diagnostics screen.
fn diag_env() -> Environment<'static> {
    let mut env = Environment::new();
    env.add_template(DIAG_TEMPLATE, include_str!("../../templates/diag.html"))
        .expect("the embedded diag.html compiles");
    env
}

pub struct Renderer {
    templates: Templates,
    /// Used in `Embedded` mode, and in `Dir` mode when the directory has no `diag.html`.
    diag: Environment<'static>,
    fonts: Fonts,
}

impl Renderer {
    /// Embedded templates are loaded now, so a broken bundle fails startup;
    /// a template directory is read lazily at render time.
    pub fn new(src: TemplateSource, dashboards: &Active) -> anyhow::Result<Renderer> {
        let templates = match src {
            TemplateSource::Embedded => Templates::Embedded(
                dashboards
                    .iter()
                    .map(|(&name, d)| (name, embedded_env(d.as_ref())))
                    .collect(),
            ),
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
        Ok(Renderer {
            templates,
            diag: diag_env(),
            fonts,
        })
    }

    /// Renders `dashboard`'s `view` into an 800x480 grayscale PNG.
    pub fn render(&self, dashboard: &str, view: &View, depth: BitDepth) -> anyhow::Result<Vec<u8>> {
        let template = view.template;
        let html = match &self.templates {
            Templates::Embedded(envs) => {
                let env = envs
                    .get(dashboard)
                    .with_context(|| format!("no templates for dashboard {dashboard}"))?;
                env.get_template(template)?.render(&view.context)
            }
            Templates::Dir(reloader) => {
                let env = reloader.acquire_env()?;
                env.get_template(template)?.render(&view.context)
            }
        }
        .with_context(|| format!("rendering template {template}"))?;

        let png = self.to_png(&html, depth)?;
        if png.len() > MAX_PNG_BYTES {
            error!(
                dashboard,
                template,
                bytes = png.len(),
                limit = MAX_PNG_BYTES,
                "rendered PNG exceeds the size limit"
            );
        }
        Ok(png)
    }

    /// Renders the diagnostics screen's `view` into an 800x480 grayscale PNG.
    /// A `diag.html` in a template directory wins over the embedded one.
    /// Fails when the PNG exceeds `MAX_PNG_BYTES`.
    pub fn render_diag(&self, view: &View, depth: BitDepth) -> anyhow::Result<Vec<u8>> {
        let template = view.template;
        let embedded = || self.diag.get_template(template)?.render(&view.context);
        let html = match &self.templates {
            Templates::Embedded(_) => embedded(),
            Templates::Dir(reloader) => {
                let env = reloader.acquire_env()?;
                match env.get_template(template) {
                    Ok(t) => t.render(&view.context),
                    Err(e) if e.kind() == minijinja::ErrorKind::TemplateNotFound => embedded(),
                    Err(e) => Err(e),
                }
            }
        }
        .with_context(|| format!("rendering template {template}"))?;

        let png = self.to_png(&html, depth)?;
        anyhow::ensure!(
            png.len() <= MAX_PNG_BYTES,
            "{template} rendered to {} bytes, over the {MAX_PNG_BYTES}-byte limit",
            png.len()
        );
        Ok(png)
    }

    /// HTML -> RGBA -> quantized PNG.
    fn to_png(&self, html: &str, depth: BitDepth) -> anyhow::Result<Vec<u8>> {
        let rgba = self.rasterize(html)?;
        Ok(encode_png(&quantize(&rgba, depth), depth))
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
    use std::sync::Arc;

    use crate::device_api::stub::StubDashboard;

    fn active() -> Active {
        let stub: Arc<dyn Dashboard> = Arc::new(StubDashboard::new("alpha"));
        Active::from([("alpha", stub)])
    }

    fn view() -> View {
        View {
            template: "stub.html",
            context: minijinja::context! { name => "alpha", now => "t" },
        }
    }

    #[test]
    fn renders_a_dashboard_through_its_own_environment() {
        let r = Renderer::new(TemplateSource::Embedded, &active()).unwrap();
        for (depth, bits) in [
            (BitDepth::Two, png::BitDepth::Two),
            (BitDepth::One, png::BitDepth::One),
        ] {
            let png = r.render("alpha", &view(), depth).unwrap();
            let d = png::Decoder::new(std::io::Cursor::new(png))
                .read_info()
                .unwrap();
            assert_eq!(
                (d.info().width, d.info().height, d.info().bit_depth),
                (800, 480, bits)
            );
        }
        assert!(r.render("bravo", &view(), BitDepth::Two).is_err());
    }

    /// Registers `shared.html` with its own body; every dashboard uses the same name.
    struct Shared(&'static str);

    impl Dashboard for Shared {
        fn load_templates(&self, env: &mut minijinja::Environment<'static>) {
            env.add_template("shared.html", self.0).unwrap();
        }
        fn spawn_pollers(
            self: Arc<Self>,
            _shutdown: tokio_util::sync::CancellationToken,
        ) -> Vec<tokio::task::JoinHandle<()>> {
            Vec::new()
        }
        fn view(&self, _now: chrono::DateTime<chrono_tz::Tz>) -> anyhow::Result<View> {
            unreachable!("not rendered through view")
        }
        fn health(&self, _now: chrono::DateTime<chrono::Utc>) -> serde_json::Value {
            serde_json::json!({})
        }
    }

    #[test]
    fn embedded_bundles_are_isolated() {
        let page = |text: &str| {
            format!(
                r#"<div style="width: 800px; height: 480px; background: white; color: black; font-family: Inter; font-size: 48px;"><p>{text}</p></div>"#
            )
        };
        let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        let one: Arc<dyn Dashboard> = Arc::new(Shared(leak(page("one"))));
        let two: Arc<dyn Dashboard> = Arc::new(Shared(leak(page("two"))));
        let r = Renderer::new(
            TemplateSource::Embedded,
            &Active::from([("alpha", one), ("bravo", two)]),
        )
        .unwrap();
        let view = View {
            template: "shared.html",
            context: minijinja::Value::UNDEFINED,
        };
        let a = r.render("alpha", &view, BitDepth::Two).unwrap();
        let b = r.render("bravo", &view, BitDepth::Two).unwrap();
        assert_ne!(a, b);
    }
}
