//! Checks `contrib/grafana/tablica.json` against what tablica actually exports:
//! every query parses as PromQL, names only metrics a sample scrape contains, and
//! uses the `Prometheus` datasource.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, TimeZone, Utc};
use chrono_tz::Tz;
use promql_parser::parser::{self, Expr};
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use tablica_core::dashboard::{Active, Dashboard, SourceState, View};
use tablica_core::history::{History, Reading};
use tablica_core::metrics::{HttpOutcome, Recorder, RenderKind, ScrapeContext};
use tablica_core::metrics::{build_registry, encode};
use tablica_core::polling::Outcome;

const DASHBOARD: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../contrib/grafana/tablica.json"
);

fn now() -> DateTime<Utc> {
    Utc.timestamp_opt(1_800_000_000, 0).unwrap()
}

/// A dashboard with one failing source; nothing else matters here.
struct FailingSource;

impl Dashboard for FailingSource {
    fn load_templates(&self, _env: &mut minijinja::Environment<'static>) {}

    fn spawn_pollers(
        self: Arc<Self>,
        _shutdown: CancellationToken,
        _recorder: Recorder,
    ) -> Vec<JoinHandle<()>> {
        Vec::new()
    }

    fn view(&self, _now: DateTime<Tz>) -> anyhow::Result<View> {
        anyhow::bail!("not rendered in this test")
    }

    fn health(&self, _now: DateTime<Utc>) -> serde_json::Value {
        serde_json::Value::Null
    }

    fn sources(&self, now: DateTime<Utc>) -> Vec<SourceState> {
        vec![SourceState {
            id: "calendar".into(),
            last_success: Some(now - Duration::hours(3)),
            failing: true,
            stale: false,
        }]
    }
}

/// A scrape with every family the dashboard can query populated.
fn sample_scrape(dir: &Path) -> String {
    let at = now();
    let clock: tablica_core::device_api::Clock = Arc::new(move || at);
    let recorder = Recorder::new(
        &[
            ("kitchen".into(), "testing".into()),
            ("hall".into(), "testing".into()),
            ("attic".into(), "testing".into()),
        ],
        &["testing"],
    );

    // Kitchen: full telemetry, with a steady drain so days-left is derived.
    let hours = 96;
    for h in 0..=hours {
        recorder.record_reading(
            "kitchen",
            &Reading {
                at: at - Duration::hours(hours - h),
                battery_v: Some(4.0 - 0.2 * h as f64 / hours as f64),
                rssi: Some(-60),
                wake_ms: Some(4000),
                image_bytes: Some(1234),
                next_refresh: Some(900),
                reset_reason: (h == hours).then(|| "ESP_RST_PANIC".into()),
                fw_version: Some("1.6.9".into()),
                model: Some("og".into()),
                ..Default::default()
            },
        );
    }
    // Hall: no battery. Attic: never seen.
    recorder.record_reading(
        "hall",
        &Reading {
            at: at - Duration::minutes(5),
            battery_v: Some(0.0),
            rssi: Some(-70),
            next_refresh: Some(900),
            ..Default::default()
        },
    );

    let tick = StdDuration::from_millis(20);
    recorder.http("/api/display", 200, HttpOutcome::Served, tick);
    recorder.render("testing", RenderKind::Device, tick, true);
    recorder.render("testing", RenderKind::Preview, tick, false);
    recorder
        .source("testing", "calendar")
        .observe(Outcome::Ok, tick);

    let mut active = Active::new();
    active.insert("testing", Arc::new(FailingSource) as Arc<dyn Dashboard>);
    let history = History::open(dir, clock.clone());
    let registry = build_registry(ScrapeContext {
        recorder,
        dashboards: Arc::new(active),
        history: Arc::new(history),
        data_dir: dir.to_owned(),
        clock,
    });
    encode(&registry).unwrap()
}

/// Metric names (with histogram/counter suffixes) present in an OpenMetrics body.
fn known_names(body: &str) -> BTreeSet<String> {
    body.lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| l.split(['{', ' ']).next())
        .map(str::to_owned)
        .collect()
}

/// The name check: known only if a scraped sample has exactly this name, so a
/// histogram is known by its `_bucket`, `_sum` and `_count` series, never bare.
fn is_known(name: &str, known: &BTreeSet<String>) -> bool {
    if name.starts_with("process_") && !cfg!(target_os = "linux") {
        return true;
    }
    known.contains(name)
}

/// Every metric name an expression selects.
fn metric_names(expr: &Expr, out: &mut BTreeSet<String>) {
    match expr {
        Expr::VectorSelector(vs) => out.extend(vs.name.clone()),
        Expr::MatrixSelector(ms) => out.extend(ms.vs.name.clone()),
        Expr::Call(call) => call.args.args.iter().for_each(|a| metric_names(a, out)),
        Expr::Aggregate(agg) => {
            metric_names(&agg.expr, out);
            if let Some(p) = &agg.param {
                metric_names(p, out);
            }
        }
        Expr::Binary(b) => {
            metric_names(&b.lhs, out);
            metric_names(&b.rhs, out);
        }
        Expr::Unary(u) => metric_names(&u.expr, out),
        Expr::Paren(p) => metric_names(&p.expr, out),
        Expr::Subquery(s) => metric_names(&s.expr, out),
        Expr::NumberLiteral(_) | Expr::StringLiteral(_) | Expr::Extension(_) => {}
    }
}

fn substitute(q: &str) -> String {
    q.replace("${device}", ".*")
        .replace("$device", ".*")
        .replace("$__range", "1d")
        .replace("$__rate_interval", "5m")
        .replace("$__interval", "1m")
}

/// Parses a query, unwrapping `label_values(<sel>, <label>)` to its selector.
fn parse(raw: &str) -> Expr {
    let q = substitute(raw);
    let q = q.trim();
    let q = match q
        .strip_prefix("label_values(")
        .and_then(|r| r.strip_suffix(')'))
    {
        Some(inner) => inner.rsplit_once(',').map_or(inner, |(sel, _)| sel).trim(),
        None => q,
    };
    parser::parse(q).unwrap_or_else(|e| panic!("{raw:?} does not parse: {e}"))
}

/// All panels, rows' nested panels included.
fn panels(json: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    for p in json["panels"].as_array().expect("panels") {
        out.push(p);
        if let Some(nested) = p["panels"].as_array() {
            out.extend(nested);
        }
    }
    out
}

fn template_query(var: &Value) -> Option<&str> {
    match &var["query"] {
        Value::String(s) => Some(s),
        Value::Object(o) => o.get("query").and_then(Value::as_str),
        _ => None,
    }
}

#[test]
fn dashboard_queries_are_valid_and_known() {
    let text = std::fs::read_to_string(DASHBOARD).expect("read contrib/grafana/tablica.json");
    let json: Value = serde_json::from_str(&text).expect("dashboard is JSON");
    assert_eq!(json["uid"], "tablica");
    assert_eq!(json["title"], "tablica");
    assert!(json["schemaVersion"].as_u64().unwrap() >= 39);

    let dir = tempfile::tempdir().unwrap();
    let known = known_names(&sample_scrape(dir.path()));

    let mut queries = Vec::new();
    for p in panels(&json) {
        let is_row = p["type"] == "row";
        if !(is_row && p.get("datasource").is_none()) {
            assert_eq!(p["datasource"], "Prometheus", "panel {}", p["title"]);
        }
        for t in p["targets"].as_array().into_iter().flatten() {
            assert_eq!(t["datasource"], "Prometheus", "target in {}", p["title"]);
            let expr = t["expr"].as_str().expect("target expr");
            queries.push((p["title"].to_string(), expr.to_owned()));
        }
    }
    for var in json["templating"]["list"].as_array().expect("templating") {
        if var["type"] == "query" {
            let q = template_query(var).expect("variable query");
            queries.push((format!("variable {}", var["name"]), q.to_owned()));
        }
    }
    assert!(queries.len() > 10, "{queries:?}");

    for (title, q) in &queries {
        assert!(q.contains(r#"job="tablica""#), "{title}: {q}");
        let mut names = BTreeSet::new();
        metric_names(&parse(q), &mut names);
        assert!(!names.is_empty(), "{title}: {q} selects no metric");
        for n in names {
            assert!(is_known(&n, &known), "{title}: unknown metric {n} in {q}");
        }
    }
}

#[test]
fn range_selector_typo_is_caught() {
    let mut names = BTreeSet::new();
    metric_names(
        &parse("increase(tablica_device_resetz_total[1d])"),
        &mut names,
    );
    assert_eq!(
        names,
        BTreeSet::from(["tablica_device_resetz_total".to_owned()])
    );
    let known = BTreeSet::from(["tablica_device_resets_total".to_owned()]);
    assert!(!is_known("tablica_device_resetz_total", &known));
    assert!(is_known("tablica_device_resets_total", &known));
}

#[test]
fn bare_histogram_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let known = known_names(&sample_scrape(dir.path()));
    assert!(is_known("tablica_render_duration_seconds_bucket", &known));
    assert!(is_known("tablica_render_duration_seconds_count", &known));
    assert!(!is_known("tablica_render_duration_seconds", &known));
}
