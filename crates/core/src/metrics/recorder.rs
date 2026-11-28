//! Event-driven metrics: counters and histograms recorded as things happen.

use std::sync::Arc;
use std::time::Duration;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

use super::{DeviceStore, MetricsSeed, labels};
use crate::history::Reading;
use crate::polling::Outcome;

const SOURCE_BUCKETS: [f64; 9] = [0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0];
const HTTP_BUCKETS: [f64; 10] = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0];
const RENDER_BUCKETS: [f64; 8] = [0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0];

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ResetLabels {
    device: String,
    reason: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SourceLabels {
    dashboard: String,
    source: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct FetchLabels {
    dashboard: String,
    source: String,
    outcome: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct HttpLabels {
    route: String,
    status: String,
    outcome: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct RouteLabels {
    route: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct RenderLabels {
    dashboard: String,
    kind: String,
}

type HistFamily<L> = Family<L, Histogram, fn() -> Histogram>;

fn source_hist() -> Histogram {
    Histogram::new(SOURCE_BUCKETS)
}
fn http_hist() -> Histogram {
    Histogram::new(HTTP_BUCKETS)
}
fn render_hist() -> Histogram {
    Histogram::new(RENDER_BUCKETS)
}

struct Inner {
    devices: DeviceStore,
    configured: Vec<(String, String)>,
    resets: Family<ResetLabels, Counter>,
    fetches: Family<FetchLabels, Counter>,
    fetch_duration: HistFamily<SourceLabels>,
    http_requests: Family<HttpLabels, Counter>,
    http_duration: HistFamily<RouteLabels>,
    render_duration: HistFamily<RenderLabels>,
    render_failures: Family<RenderLabels, Counter>,
}

/// Records metric events. Cheap to clone; safe to share across tasks.
#[derive(Clone)]
pub struct Recorder(Arc<Inner>);

/// Per-source fetch recording, with its series already created.
#[derive(Clone)]
pub struct SourceMetrics {
    dashboard: String,
    source: String,
    inner: Arc<Inner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HttpOutcome {
    Served,
    Refused,
    Unavailable,
    #[default]
    None,
}

impl HttpOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            HttpOutcome::Served => "served",
            HttpOutcome::Refused => "refused",
            HttpOutcome::Unavailable => "unavailable",
            HttpOutcome::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderKind {
    Device,
    Preview,
    Diag,
}

impl RenderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RenderKind::Device => "device",
            RenderKind::Preview => "preview",
            RenderKind::Diag => "diag",
        }
    }
}

fn outcome_str(o: Outcome) -> &'static str {
    match o {
        Outcome::Ok => "ok",
        Outcome::Failed => "failed",
        Outcome::Rejected => "rejected",
    }
}

fn fetch_labels(dashboard: &str, source: &str, outcome: &str) -> FetchLabels {
    FetchLabels {
        dashboard: dashboard.to_owned(),
        source: source.to_owned(),
        outcome: outcome.to_owned(),
    }
}

fn source_labels(dashboard: &str, source: &str) -> SourceLabels {
    SourceLabels {
        dashboard: dashboard.to_owned(),
        source: source.to_owned(),
    }
}

impl Recorder {
    /// `devices` is `(name, dashboard)` per configured device.
    pub fn new(devices: &[(String, String)]) -> Recorder {
        let inner = Inner {
            devices: DeviceStore::default(),
            configured: devices.to_vec(),
            resets: Family::default(),
            fetches: Family::default(),
            fetch_duration: Family::new_with_constructor(source_hist as fn() -> Histogram),
            http_requests: Family::default(),
            http_duration: Family::new_with_constructor(http_hist as fn() -> Histogram),
            render_duration: Family::new_with_constructor(render_hist as fn() -> Histogram),
            render_failures: Family::default(),
        };
        for (name, _) in devices {
            for reason in labels::counted_reset_labels() {
                let _ = inner.resets.get_or_create_owned(&ResetLabels {
                    device: name.clone(),
                    reason: reason.to_owned(),
                });
            }
        }
        Recorder(Arc::new(inner))
    }

    pub fn devices(&self) -> &DeviceStore {
        &self.0.devices
    }

    pub fn configured(&self) -> &[(String, String)] {
        &self.0.configured
    }

    /// Record a live check-in; counts a reset when the reason is one worth counting.
    pub fn record_reading(&self, device: &str, r: &Reading) {
        self.0.devices.record(device, r);
        if let Some(reason) = labels::counted_reset(r.reset_reason.as_deref()) {
            self.0
                .resets
                .get_or_create_owned(&ResetLabels {
                    device: device.to_owned(),
                    reason: reason.to_owned(),
                })
                .inc();
        }
    }

    /// Restore state from history. Never touches counters.
    pub fn seed(&self, device: &str, seed: MetricsSeed) {
        self.0.devices.seed(device, seed);
    }

    pub fn source(&self, dashboard: &str, source: &str) -> SourceMetrics {
        for o in [Outcome::Ok, Outcome::Failed, Outcome::Rejected] {
            let _ = self.0.fetches.get_or_create_owned(&fetch_labels(
                dashboard,
                source,
                outcome_str(o),
            ));
        }
        let _ = self
            .0
            .fetch_duration
            .get_or_create_owned(&source_labels(dashboard, source));
        SourceMetrics {
            dashboard: dashboard.to_owned(),
            source: source.to_owned(),
            inner: self.0.clone(),
        }
    }

    pub fn http(&self, route: &str, status: u16, outcome: HttpOutcome, elapsed: Duration) {
        self.0
            .http_requests
            .get_or_create_owned(&HttpLabels {
                route: route.to_owned(),
                status: status.to_string(),
                outcome: outcome.as_str().to_owned(),
            })
            .inc();
        self.0
            .http_duration
            .get_or_create_owned(&RouteLabels {
                route: route.to_owned(),
            })
            .observe(elapsed.as_secs_f64());
    }

    pub fn render(&self, dashboard: &str, kind: RenderKind, elapsed: Duration, ok: bool) {
        let l = RenderLabels {
            dashboard: dashboard.to_owned(),
            kind: kind.as_str().to_owned(),
        };
        if ok {
            self.0
                .render_duration
                .get_or_create_owned(&l)
                .observe(elapsed.as_secs_f64());
        } else {
            self.0.render_failures.get_or_create_owned(&l).inc();
        }
    }

    pub fn register(&self, registry: &mut Registry) {
        let i = &self.0;
        registry.register(
            "tablica_device_resets",
            "Device check-ins that reported a counted reset reason",
            i.resets.clone(),
        );
        registry.register(
            "tablica_source_fetches",
            "Data source fetches by outcome",
            i.fetches.clone(),
        );
        registry.register(
            "tablica_source_fetch_duration_seconds",
            "Data source fetch duration in seconds",
            i.fetch_duration.clone(),
        );
        registry.register(
            "tablica_http_requests",
            "HTTP requests by route, status and outcome",
            i.http_requests.clone(),
        );
        registry.register(
            "tablica_http_request_duration_seconds",
            "HTTP request duration in seconds",
            i.http_duration.clone(),
        );
        registry.register(
            "tablica_render_duration_seconds",
            "Successful dashboard render duration in seconds",
            i.render_duration.clone(),
        );
        registry.register(
            "tablica_render_failures",
            "Failed dashboard renders",
            i.render_failures.clone(),
        );
    }
}

impl SourceMetrics {
    pub fn observe(&self, outcome: Outcome, elapsed: Duration) {
        self.inner
            .fetches
            .get_or_create_owned(&fetch_labels(
                &self.dashboard,
                &self.source,
                outcome_str(outcome),
            ))
            .inc();
        self.inner
            .fetch_duration
            .get_or_create_owned(&source_labels(&self.dashboard, &self.source))
            .observe(elapsed.as_secs_f64());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus_client::encoding::text::encode;

    fn recorder() -> (Recorder, Registry) {
        let r = Recorder::new(&[("kitchen".into(), "family".into())]);
        let mut reg = Registry::default();
        r.register(&mut reg);
        (r, reg)
    }

    fn text(reg: &Registry) -> String {
        let mut s = String::new();
        encode(&mut s, reg).unwrap();
        s
    }

    fn reset(reason: &str) -> Reading {
        Reading {
            reset_reason: Some(reason.into()),
            ..Default::default()
        }
    }

    #[test]
    fn resets_zero_initialised() {
        let (_r, reg) = recorder();
        let t = text(&reg);
        assert!(
            t.contains(r#"tablica_device_resets_total{device="kitchen",reason="ESP_RST_SW"} 0"#),
            "{t}"
        );
        assert!(!t.contains("ESP_RST_DEEPSLEEP"), "{t}");
    }

    #[test]
    fn reset_counted_once_per_checkin() {
        let (r, reg) = recorder();
        r.record_reading("kitchen", &reset("ESP_RST_BROWNOUT"));
        r.record_reading("kitchen", &reset("ESP_RST_DEEPSLEEP"));
        let t = text(&reg);
        assert!(
            t.contains(
                r#"tablica_device_resets_total{device="kitchen",reason="ESP_RST_BROWNOUT"} 1"#
            ),
            "{t}"
        );
        assert!(
            t.contains(r#"tablica_device_resets_total{device="kitchen",reason="ESP_RST_SW"} 0"#),
            "{t}"
        );
    }

    #[test]
    fn seed_does_not_count() {
        let (r, reg) = recorder();
        r.seed(
            "kitchen",
            MetricsSeed {
                last_reset: Some(reset("ESP_RST_BROWNOUT")),
                ..Default::default()
            },
        );
        let t = text(&reg);
        assert!(
            t.contains(
                r#"tablica_device_resets_total{device="kitchen",reason="ESP_RST_BROWNOUT"} 0"#
            ),
            "{t}"
        );
    }

    #[test]
    fn source_series_preinitialised() {
        let (r, reg) = recorder();
        let _s = r.source("family", "weather");
        let t = text(&reg);
        for o in ["ok", "failed", "rejected"] {
            let line = format!(
                r#"tablica_source_fetches_total{{dashboard="family",source="weather",outcome="{o}"}} 0"#
            );
            assert!(t.contains(&line), "{line}\n{t}");
        }
        assert!(
            t.contains(r#"tablica_source_fetch_duration_seconds_count{dashboard="family",source="weather"} 0"#),
            "{t}"
        );
    }

    #[test]
    fn source_observe_records() {
        let (r, reg) = recorder();
        let s = r.source("family", "weather");
        s.observe(Outcome::Failed, Duration::from_millis(200));
        let t = text(&reg);
        assert!(t.contains(r#"tablica_source_fetches_total{dashboard="family",source="weather",outcome="failed"} 1"#), "{t}");
        assert!(t.contains(r#"tablica_source_fetch_duration_seconds_count{dashboard="family",source="weather"} 1"#), "{t}");
    }

    fn value(t: &str, prefix: &str) -> f64 {
        let line = t
            .lines()
            .find(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix}\n{t}"));
        line.rsplit(' ').next().unwrap().parse().unwrap()
    }

    #[test]
    fn http_and_render_recorded() {
        let (r, reg) = recorder();
        r.http(
            "/api/display",
            200,
            HttpOutcome::Served,
            Duration::from_millis(30),
        );
        let t = text(&reg);
        assert!(t.contains(r#"tablica_http_requests_total{route="/api/display",status="200",outcome="served"} 1"#), "{t}");
        let sum = value(
            &t,
            r#"tablica_http_request_duration_seconds_sum{route="/api/display"}"#,
        );
        assert!((sum - 0.03).abs() < 1e-9, "{sum}");
        assert_eq!(
            value(
                &t,
                r#"tablica_http_request_duration_seconds_bucket{le="0.05",route="/api/display"}"#
            ),
            1.0
        );
        assert_eq!(
            value(
                &t,
                r#"tablica_http_request_duration_seconds_bucket{le="0.025",route="/api/display"}"#
            ),
            0.0
        );

        r.render(
            "family",
            RenderKind::Device,
            Duration::from_millis(500),
            false,
        );
        let t = text(&reg);
        assert!(
            t.contains(r#"tablica_render_failures_total{dashboard="family",kind="device"} 1"#),
            "{t}"
        );
        assert!(!t.contains("tablica_render_duration_seconds_count"), "{t}");
    }
}
