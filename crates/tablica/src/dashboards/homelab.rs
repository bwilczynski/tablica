//! Homelab dashboard view model (placeholder until the dashboard exists).

use serde::Serialize;

use crate::config::Secrets;
use std::sync::Arc;

use chrono::{DateTime, NaiveTime, Utc};
use chrono_tz::Tz;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::dashboard::{
    Dashboard, DashboardEntry, ParseResult, ParsedDashboard, PrepareContext, View,
};
use crate::text::pl;

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
pub struct HomelabView {
    pub message: &'static str,
}

pub fn build() -> HomelabView {
    HomelabView {
        message: pl::HOMELAB_PLACEHOLDER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_placeholder() {
        assert_eq!(build().message, "Homelab: wkrótce");
    }

    #[test]
    fn homelab_view_is_placeholder() {
        let now = crate::dashboards::test_support::warsaw(2026, 11, 25, 7, 0);
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
}
