//! Scrape-time metrics: gauges derived from current state on each scrape.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use prometheus_client::collector::Collector;
use prometheus_client::encoding::{DescriptorEncoder, EncodeLabelSet, EncodeMetric};
use prometheus_client::metrics::counter::ConstCounter;
use prometheus_client::metrics::gauge::ConstGauge;

use super::devices::battery_at;
use super::{DeviceState, Recorder};
use crate::dashboard::{Active, SourceState};
use crate::device_api::Clock;
use crate::diagnostics::battery::{Estimate, Power};
use crate::history::History;

/// Everything the scrape-time collector reads.
pub struct ScrapeContext {
    pub recorder: Recorder,
    pub dashboards: Arc<Active>,
    pub history: Arc<History>,
    pub data_dir: PathBuf,
    pub clock: Clock,
}

pub(super) struct ScrapeCollector {
    ctx: ScrapeContext,
}

impl ScrapeCollector {
    pub(super) fn new(ctx: ScrapeContext) -> Self {
        Self { ctx }
    }
}

impl fmt::Debug for ScrapeCollector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScrapeCollector")
            .field("data_dir", &self.ctx.data_dir)
            .finish_non_exhaustive()
    }
}

type Labels<'a, const N: usize> = [(&'static str, &'a str); N];

/// Encodes one family, or nothing when it has no samples.
fn family<L, M>(
    enc: &mut DescriptorEncoder,
    name: &str,
    help: &str,
    samples: &[(L, M)],
) -> fmt::Result
where
    L: EncodeLabelSet,
    M: EncodeMetric,
{
    let Some((_, first)) = samples.first() else {
        return Ok(());
    };
    let mut fam = enc.encode_descriptor(name, help, None, first.metric_type())?;
    for (labels, metric) in samples {
        metric.encode(fam.encode_family(labels)?)?;
    }
    Ok(())
}

/// Encodes one unlabelled metric.
fn single(
    enc: &mut DescriptorEncoder,
    name: &str,
    help: &str,
    m: impl EncodeMetric,
) -> fmt::Result {
    m.encode(enc.encode_descriptor(name, help, None, m.metric_type())?)
}

fn seconds(at: DateTime<Utc>) -> f64 {
    at.timestamp_millis() as f64 / 1000.0
}

fn bool_gauge(b: bool) -> ConstGauge<i64> {
    ConstGauge::new(i64::from(b))
}

/// Device gauge samples, one list per family.
#[derive(Default)]
struct DeviceGauges<'a> {
    last_seen: Vec<(Labels<'a, 1>, ConstGauge<f64>)>,
    next_checkin: Vec<(Labels<'a, 1>, ConstGauge<f64>)>,
    volts: Vec<(Labels<'a, 1>, ConstGauge<f64>)>,
    percent: Vec<(Labels<'a, 1>, ConstGauge<f64>)>,
    days_left: Vec<(Labels<'a, 1>, ConstGauge<f64>)>,
    rssi: Vec<(Labels<'a, 1>, ConstGauge<i64>)>,
    wake: Vec<(Labels<'a, 1>, ConstGauge<f64>)>,
    image: Vec<(Labels<'a, 1>, ConstGauge<i64>)>,
    last_reset: Vec<(Labels<'a, 1>, ConstGauge<f64>)>,
}

impl ScrapeCollector {
    fn encode_devices(
        &self,
        enc: &mut DescriptorEncoder,
        now: DateTime<Utc>,
        states: &[(&str, DeviceState)],
    ) -> fmt::Result {
        let configured: Vec<_> = self
            .ctx
            .recorder
            .configured()
            .iter()
            .map(|(d, b)| {
                (
                    [("device", d.as_str()), ("dashboard", b.as_str())],
                    ConstGauge::new(1i64),
                )
            })
            .collect();
        family(
            enc,
            "tablica_device_configured",
            "Configured devices and their dashboard.",
            &configured,
        )?;

        let mut g = DeviceGauges::default();
        let mut info = Vec::new();
        for (device, st) in states {
            let l = [("device", *device)];
            if let Some(at) = st.last_seen {
                g.last_seen.push((l, ConstGauge::new(seconds(at))));
            }
            if let Some((at, next)) = st.last_reply {
                let due =
                    chrono::TimeDelta::try_seconds(next).and_then(|d| at.checked_add_signed(d));
                if let Some(due) = due {
                    g.next_checkin.push((l, ConstGauge::new(seconds(due))));
                }
            }
            if let Power::Battery {
                volts,
                percent,
                estimate,
            } = battery_at(st, now)
            {
                g.volts.push((l, ConstGauge::new(volts)));
                g.percent.push((l, ConstGauge::new(percent)));
                if let Estimate::DaysLeft(days) = estimate {
                    g.days_left.push((l, ConstGauge::new(days)));
                }
            }
            if let Some((_, Some(rssi))) = st.rssi {
                g.rssi.push((l, ConstGauge::new(rssi)));
            }
            if let Some((_, ms)) = st.last_wake {
                g.wake.push((l, ConstGauge::new(ms as f64 / 1000.0)));
            }
            if let Some((_, bytes)) = st.last_image_bytes {
                g.image.push((l, ConstGauge::new(bytes)));
            }
            if let Some(at) = st.last_reset {
                g.last_reset.push((l, ConstGauge::new(seconds(at))));
            }
            if let Some((_, model, fw)) = &st.info {
                info.push((
                    [
                        ("device", *device),
                        ("model", model.unwrap_or("")),
                        ("fw_version", fw.as_deref().unwrap_or("")),
                    ],
                    ConstGauge::new(1i64),
                ));
            }
        }

        family(
            enc,
            "tablica_device_last_seen_timestamp_seconds",
            "Time of the newest check-in.",
            &g.last_seen,
        )?;
        family(
            enc,
            "tablica_device_next_checkin_timestamp_seconds",
            "When the device was told to check in next.",
            &g.next_checkin,
        )?;
        family(
            enc,
            "tablica_device_battery_volts",
            "Battery voltage (median of recent readings).",
            &g.volts,
        )?;
        family(
            enc,
            "tablica_device_battery_percent",
            "Estimated battery charge in percent.",
            &g.percent,
        )?;
        family(
            enc,
            "tablica_device_battery_days_left",
            "Estimated days of battery left.",
            &g.days_left,
        )?;
        family(
            enc,
            "tablica_device_wifi_rssi_dbm",
            "Wi-Fi signal strength of the newest check-in.",
            &g.rssi,
        )?;
        family(
            enc,
            "tablica_device_wake_seconds",
            "Last reported non-zero wake duration in seconds.",
            &g.wake,
        )?;
        family(
            enc,
            "tablica_device_image_bytes",
            "Size of the last rendered image in bytes.",
            &g.image,
        )?;
        family(
            enc,
            "tablica_device_last_reset_timestamp_seconds",
            "Time of the newest check-in reporting a counted reset.",
            &g.last_reset,
        )?;
        family(
            enc,
            "tablica_device_info",
            "Device model and firmware version from the newest check-in.",
            &info,
        )
    }

    fn encode_sources(&self, enc: &mut DescriptorEncoder, now: DateTime<Utc>) -> fmt::Result {
        let per_dashboard: Vec<(&str, Vec<SourceState>)> = self
            .ctx
            .dashboards
            .iter()
            .map(|(name, d)| (*name, d.sources(now)))
            .collect();
        let mut last_success = Vec::new();
        let mut failing = Vec::new();
        let mut stale = Vec::new();
        for (dashboard, sources) in &per_dashboard {
            for s in sources {
                let l = [("dashboard", *dashboard), ("source", s.id.as_str())];
                if let Some(at) = s.last_success {
                    last_success.push((l, ConstGauge::new(seconds(at))));
                }
                failing.push((l, bool_gauge(s.failing)));
                stale.push((l, bool_gauge(s.stale)));
            }
        }
        family(
            enc,
            "tablica_source_last_success_timestamp_seconds",
            "Time of the source's last successful fetch.",
            &last_success,
        )?;
        family(
            enc,
            "tablica_source_failing",
            "1 when the source's latest fetch failed.",
            &failing,
        )?;
        family(
            enc,
            "tablica_source_stale",
            "1 when the source has no data or its data is too old.",
            &stale,
        )
    }

    fn encode_history(&self, enc: &mut DescriptorEncoder) -> fmt::Result {
        let history = &self.ctx.history;
        single(
            enc,
            "tablica_history_enabled",
            "1 when the history store is available.",
            bool_gauge(history.is_available()),
        )?;
        if let Ok(meta) = std::fs::metadata(self.ctx.data_dir.join("history.db")) {
            single(
                enc,
                "tablica_history_db_bytes",
                "Size of the history database file in bytes.",
                ConstGauge::new(meta.len()),
            )?;
        }
        single(
            enc,
            "tablica_history_dropped",
            "History records dropped because the queue or byte budget was full.",
            ConstCounter::new(history.dropped()),
        )
    }
}

impl Collector for ScrapeCollector {
    fn encode(&self, mut enc: DescriptorEncoder) -> fmt::Result {
        let now = (self.ctx.clock)();
        // `get` clones under a short lock; battery derivation runs on the clones.
        let devices = self.ctx.recorder.devices();
        let states: Vec<(&str, DeviceState)> = self
            .ctx
            .recorder
            .configured()
            .iter()
            .filter_map(|(name, _)| Some((name.as_str(), devices.get(name)?)))
            .collect();
        self.encode_devices(&mut enc, now, &states)?;
        self.encode_sources(&mut enc, now)?;
        self.encode_history(&mut enc)?;
        family(
            &mut enc,
            "tablica_build_info",
            "Build information.",
            &[(
                [("version", env!("CARGO_PKG_VERSION"))],
                ConstGauge::new(1i64),
            )],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::{Dashboard, SourceState, View};
    use crate::device_api::stub::StubDashboard;
    use crate::history::Reading;
    use crate::metrics::{build_registry, encode};
    use chrono::{Duration, TimeZone};
    use chrono_tz::Tz;
    use std::path::Path;
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;

    fn now() -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000, 0).unwrap()
    }

    fn configured() -> Vec<(String, String)> {
        vec![
            ("kitchen".into(), "stub".into()),
            ("hall".into(), "stub".into()),
        ]
    }

    fn stub_active() -> Active {
        let mut a = Active::new();
        a.insert(
            "stub",
            Arc::new(StubDashboard::new("stub")) as Arc<dyn Dashboard>,
        );
        a
    }

    fn scrape(recorder: &Recorder, active: Active, history: History, dir: &Path) -> String {
        let at = now();
        let reg = build_registry(ScrapeContext {
            recorder: recorder.clone(),
            dashboards: Arc::new(active),
            history: Arc::new(history),
            data_dir: dir.to_owned(),
            clock: Arc::new(move || at),
        });
        encode(&reg).unwrap()
    }

    /// The value of the one sample whose name and labels are exactly `series`.
    fn value(body: &str, series: &str) -> Option<f64> {
        let prefix = format!("{series} ");
        let mut hits = body.lines().filter_map(|l| l.strip_prefix(prefix.as_str()));
        let v = hits.next()?;
        assert!(hits.next().is_none(), "{series} appears twice\n{body}");
        Some(v.parse().unwrap())
    }

    fn lines_starting<'a>(body: &'a str, name: &str) -> Vec<&'a str> {
        body.lines()
            .filter(|l| l.starts_with(&format!("{name}{{")) || l.starts_with(&format!("{name} ")))
            .collect()
    }

    #[test]
    fn scrape_contains_inventory_for_never_seen() {
        let dir = tempfile::tempdir().unwrap();
        let r = Recorder::new(&configured(), &["stub"]);
        let body = scrape(&r, stub_active(), History::disabled(), dir.path());
        assert_eq!(
            lines_starting(&body, "tablica_device_configured").len(),
            2,
            "{body}"
        );
        assert_eq!(
            value(
                &body,
                r#"tablica_device_configured{device="kitchen",dashboard="stub"}"#
            ),
            Some(1.0)
        );
        assert_eq!(
            value(
                &body,
                r#"tablica_device_configured{device="hall",dashboard="stub"}"#
            ),
            Some(1.0)
        );
        assert!(
            !body.contains("tablica_device_last_seen_timestamp_seconds{"),
            "{body}"
        );
        assert!(!body.contains("tablica_device_battery_volts{"), "{body}");
        assert!(!body.contains("tablica_device_info{"), "{body}");
        assert_eq!(
            value(
                &body,
                &format!(
                    r#"tablica_build_info{{version="{}"}}"#,
                    env!("CARGO_PKG_VERSION")
                )
            ),
            Some(1.0)
        );
    }

    #[test]
    fn scrape_omits_next_checkin_when_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let r = Recorder::new(&configured(), &["stub"]);
        let at = now() - Duration::minutes(10);
        for (device, next) in [("kitchen", i64::MAX), ("hall", 900)] {
            r.record_reading(
                device,
                &Reading {
                    at,
                    next_refresh: Some(next),
                    ..Default::default()
                },
            );
        }
        let body = scrape(&r, stub_active(), History::disabled(), dir.path());
        let ts = at.timestamp() as f64;
        assert_eq!(
            value(
                &body,
                r#"tablica_device_next_checkin_timestamp_seconds{device="kitchen"}"#
            ),
            None,
            "{body}"
        );
        assert_eq!(
            value(
                &body,
                r#"tablica_device_next_checkin_timestamp_seconds{device="hall"}"#
            ),
            Some(ts + 900.0),
            "{body}"
        );
        assert_eq!(
            value(
                &body,
                r#"tablica_device_last_seen_timestamp_seconds{device="kitchen"}"#
            ),
            Some(ts),
            "{body}"
        );
    }

    #[test]
    fn scrape_device_state() {
        let dir = tempfile::tempdir().unwrap();
        let r = Recorder::new(&configured(), &["stub"]);
        let at = now() - Duration::minutes(10);
        r.record_reading(
            "kitchen",
            &Reading {
                at,
                battery_v: Some(4.0),
                rssi: Some(-60),
                wake_ms: Some(4000),
                image_bytes: Some(1234),
                next_refresh: Some(900),
                reset_reason: Some("ESP_RST_PANIC".into()),
                ..Default::default()
            },
        );
        let body = scrape(&r, stub_active(), History::disabled(), dir.path());
        let d = r#"{device="kitchen"}"#;
        let g = |name: &str| value(&body, &format!("tablica_device_{name}{d}"));
        let ts = at.timestamp() as f64;
        assert_eq!(g("last_seen_timestamp_seconds"), Some(ts), "{body}");
        assert_eq!(
            g("next_checkin_timestamp_seconds"),
            Some(ts + 900.0),
            "{body}"
        );
        assert_eq!(g("battery_volts"), Some(4.0), "{body}");
        assert!(
            g("battery_percent").is_some_and(|p| p > 0.0 && p <= 100.0),
            "{body}"
        );
        assert_eq!(g("battery_days_left"), None, "{body}");
        assert_eq!(g("wifi_rssi_dbm"), Some(-60.0), "{body}");
        assert_eq!(g("wake_seconds"), Some(4.0), "{body}");
        assert_eq!(g("image_bytes"), Some(1234.0), "{body}");
        assert_eq!(g("last_reset_timestamp_seconds"), Some(ts), "{body}");
        assert!(!body.contains(r#"{device="hall"} "#), "{body}");
    }

    #[test]
    fn scrape_with_history_disabled() {
        let r = Recorder::new(&configured(), &["stub"]);

        let dir = tempfile::tempdir().unwrap();
        let body = scrape(&r, stub_active(), History::disabled(), dir.path());
        assert_eq!(value(&body, "tablica_history_enabled"), Some(0.0), "{body}");
        assert!(!body.contains("tablica_history_db_bytes"), "{body}");
        assert_eq!(
            value(&body, "tablica_history_dropped_total"),
            Some(0.0),
            "{body}"
        );
        assert_eq!(
            lines_starting(&body, "tablica_device_configured").len(),
            2,
            "{body}"
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("history.db"), [0u8; 777]).unwrap();
        let body = scrape(&r, stub_active(), History::disabled(), dir.path());
        assert_eq!(value(&body, "tablica_history_enabled"), Some(0.0), "{body}");
        assert_eq!(
            value(&body, "tablica_history_db_bytes"),
            Some(777.0),
            "{body}"
        );
    }

    struct Sourced;

    impl Dashboard for Sourced {
        fn load_templates(&self, _env: &mut minijinja::Environment<'static>) {}
        fn spawn_pollers(
            self: Arc<Self>,
            _shutdown: CancellationToken,
            _recorder: Recorder,
        ) -> Vec<JoinHandle<()>> {
            Vec::new()
        }
        fn view(&self, _now: DateTime<Tz>) -> anyhow::Result<View> {
            anyhow::bail!("no view")
        }
        fn health(&self, _now: DateTime<Utc>) -> serde_json::Value {
            serde_json::Value::Null
        }
        fn sources(&self, now: DateTime<Utc>) -> Vec<SourceState> {
            vec![
                SourceState {
                    id: "weather".into(),
                    last_success: Some(now - Duration::hours(5)),
                    failing: true,
                    stale: true,
                },
                SourceState {
                    id: "calendar".into(),
                    last_success: None,
                    failing: false,
                    stale: false,
                },
            ]
        }
    }

    #[test]
    fn scrape_sources() {
        let dir = tempfile::tempdir().unwrap();
        let r = Recorder::new(&configured(), &["stub"]);
        let mut active = stub_active();
        active.insert("family", Arc::new(Sourced) as Arc<dyn Dashboard>);
        let body = scrape(&r, active, History::disabled(), dir.path());
        let w = r#"{dashboard="family",source="weather"}"#;
        let c = r#"{dashboard="family",source="calendar"}"#;
        assert_eq!(
            value(&body, &format!("tablica_source_failing{w}")),
            Some(1.0),
            "{body}"
        );
        assert_eq!(
            value(&body, &format!("tablica_source_stale{w}")),
            Some(1.0),
            "{body}"
        );
        assert_eq!(
            value(
                &body,
                &format!("tablica_source_last_success_timestamp_seconds{w}")
            ),
            Some((now() - Duration::hours(5)).timestamp() as f64),
            "{body}"
        );
        assert_eq!(
            value(&body, &format!("tablica_source_failing{c}")),
            Some(0.0),
            "{body}"
        );
        assert_eq!(
            value(&body, &format!("tablica_source_stale{c}")),
            Some(0.0),
            "{body}"
        );
        assert_eq!(
            value(
                &body,
                &format!("tablica_source_last_success_timestamp_seconds{c}")
            ),
            None,
            "{body}"
        );
    }

    #[test]
    fn scrape_sanitises_garbage_labels() {
        let dir = tempfile::tempdir().unwrap();
        let r = Recorder::new(&configured(), &["stub"]);
        r.record_reading(
            "kitchen",
            &Reading {
                at: now(),
                model: Some("x\"\n".into()),
                fw_version: Some("1.2.3\nevil".into()),
                ..Default::default()
            },
        );
        let body = scrape(&r, stub_active(), History::disabled(), dir.path());
        assert!(
            body.lines().any(|l| l
                == r#"tablica_device_info{device="kitchen",model="other",fw_version="other"} 1"#),
            "{body}"
        );
        assert!(!body.contains("evil"), "{body}");
        for l in body.lines() {
            assert!(
                l.matches('{').count() == l.matches('}').count(),
                "split label in {l:?}"
            );
        }
        assert!(body.trim_end().ends_with("# EOF"), "{body}");
        assert_eq!(body.matches("# EOF").count(), 1, "{body}");
    }

    #[test]
    fn info_absent_fields_encode_empty() {
        let dir = tempfile::tempdir().unwrap();
        let r = Recorder::new(&configured(), &["stub"]);
        r.record_reading(
            "kitchen",
            &Reading {
                at: now(),
                model: Some("og".into()),
                ..Default::default()
            },
        );
        let body = scrape(&r, stub_active(), History::disabled(), dir.path());
        assert!(
            body.lines().any(|l| l
                == r#"tablica_device_info{device="kitchen",model="og",fw_version=""} 1"#),
            "{body}"
        );
    }
}
