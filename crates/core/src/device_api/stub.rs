//! A minimal dashboard for core tests: one inline template, counters, no data.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use chrono::{DateTime, NaiveTime, Utc};
use chrono_tz::Tz;
use serde_json::json;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::dashboard::{Dashboard, View};
use crate::diagnostics::Section;

/// Shows its name and the render time, so renders at different times differ.
const TEMPLATE: &str = r#"<div style="width: 800px; height: 480px; background: white; color: black; font-family: Inter; font-size: 48px;"><p>{{ name }}</p><p>{{ now }}</p></div>"#;

pub struct StubDashboard {
    pub name: &'static str,
    /// When set, `view` fails.
    pub fail_render: AtomicBool,
    /// How many times `spawn_pollers` was called.
    pub spawned: AtomicUsize,
    pub switch_at: Option<NaiveTime>,
    /// The `now` of the last `diagnostics` call.
    pub diag_now: Mutex<Option<DateTime<Utc>>>,
}

impl StubDashboard {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            fail_render: AtomicBool::new(false),
            spawned: AtomicUsize::new(0),
            switch_at: None,
            diag_now: Mutex::new(None),
        }
    }
}

impl Dashboard for StubDashboard {
    fn load_templates(&self, env: &mut minijinja::Environment<'static>) {
        env.add_template("stub.html", TEMPLATE)
            .expect("stub template is valid");
    }

    fn spawn_pollers(
        self: Arc<Self>,
        _shutdown: CancellationToken,
        _recorder: crate::metrics::Recorder,
    ) -> Vec<JoinHandle<()>> {
        self.spawned.fetch_add(1, Ordering::SeqCst);
        Vec::new()
    }

    fn view(&self, now: DateTime<Tz>) -> anyhow::Result<View> {
        if self.fail_render.load(Ordering::SeqCst) {
            anyhow::bail!("{} render failure requested", self.name);
        }
        Ok(View {
            template: "stub.html",
            context: minijinja::context! { name => self.name, now => now.to_rfc3339() },
        })
    }

    fn switch_at(&self) -> Option<NaiveTime> {
        self.switch_at
    }

    fn diagnostics(&self, now: DateTime<Utc>) -> Vec<Section> {
        *self.diag_now.lock().unwrap() = Some(now);
        Vec::new()
    }

    fn health(&self, _now: DateTime<Utc>) -> serde_json::Value {
        json!({ "stub": self.name })
    }
}
