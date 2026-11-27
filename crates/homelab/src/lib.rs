//! Homelab dashboard (placeholder until the dashboard exists).

use serde::Serialize;

use std::sync::Arc;
use tablica_core::config::Secrets;

use chrono::{DateTime, NaiveTime, Utc};
use chrono_tz::Tz;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use tablica_core::dashboard::{
    Dashboard, DashboardEntry, ParseResult, ParsedDashboard, PrepareContext, View,
};

/// What the placeholder template shows.
const HOMELAB_PLACEHOLDER: &str = "Homelab: wkrótce";

/// Homelab takes no config; any `[homelab]` keys are ignored.
pub struct ParsedHomelab;

impl ParsedDashboard for ParsedHomelab {
    fn prepare(self: Box<Self>, _ctx: &PrepareContext) -> anyhow::Result<Arc<dyn Dashboard>> {
        Ok(Arc::new(HomelabDashboard))
    }
}

/// The placeholder dashboard: no caches, no pollers.
pub struct HomelabDashboard;

impl Dashboard for HomelabDashboard {
    fn load_templates(&self, env: &mut minijinja::Environment<'static>) {
        minijinja_embed::load_templates!(*env);
    }

    fn spawn_pollers(self: Arc<Self>, _shutdown: CancellationToken) -> Vec<JoinHandle<()>> {
        Vec::new()
    }

    fn view(&self, _now: DateTime<Tz>) -> anyhow::Result<View> {
        Ok(View {
            template: "homelab.html",
            context: minijinja::Value::from_serialize(build()),
        })
    }

    fn switch_at(&self) -> Option<NaiveTime> {
        None
    }

    fn health(&self, _now: DateTime<Utc>) -> serde_json::Value {
        serde_json::json!({})
    }
}

/// Always active.
pub fn entry() -> DashboardEntry {
    DashboardEntry {
        name: "homelab",
        parse,
    }
}

fn parse(_section: Option<&toml::Value>, _secrets: &Secrets) -> ParseResult {
    Ok(Some(Box::new(ParsedHomelab)))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct HomelabView {
    pub message: &'static str,
}

fn build() -> HomelabView {
    HomelabView {
        message: HOMELAB_PLACEHOLDER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn shows_placeholder() {
        assert_eq!(build().message, "Homelab: wkrótce");
    }

    #[test]
    fn homelab_view_is_placeholder() {
        let now = chrono_tz::Europe::Warsaw
            .with_ymd_and_hms(2026, 11, 25, 7, 0, 0)
            .unwrap();
        let view = HomelabDashboard.view(now).unwrap();
        assert_eq!(view.template, "homelab.html");
        assert_eq!(
            view.context.get_attr("message").unwrap().to_string(),
            "Homelab: wkrótce"
        );
        assert_eq!(
            HomelabDashboard.health(now.with_timezone(&Utc)),
            serde_json::json!({})
        );
        assert!(HomelabDashboard.switch_at().is_none());
    }

    #[test]
    fn diagnostics_is_empty() {
        assert!(HomelabDashboard.diagnostics(Utc::now()).is_empty());
    }

    #[test]
    fn embedded_bundle_renders_a_valid_png() {
        use tablica_core::config::BitDepth;
        use tablica_core::dashboard::Active;
        use tablica_core::render::{Renderer, TemplateSource};

        let dashboard: Arc<dyn Dashboard> = Arc::new(HomelabDashboard);
        let now = chrono_tz::Europe::Warsaw
            .with_ymd_and_hms(2026, 11, 25, 7, 0, 0)
            .unwrap();
        let view = dashboard.view(now).unwrap();
        let r = Renderer::new(
            TemplateSource::Embedded,
            &Active::from([("homelab", dashboard)]),
        )
        .unwrap();
        let png = r.render("homelab", &view, BitDepth::Two).unwrap();
        let info = png::Decoder::new(std::io::Cursor::new(png))
            .read_info()
            .unwrap();
        let info = info.info();
        assert_eq!(
            (info.width, info.height, info.bit_depth),
            (800, 480, png::BitDepth::Two)
        );
    }
}
