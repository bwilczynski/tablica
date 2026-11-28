# Prometheus Metrics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve Prometheus metrics for tablica and its TRMNL devices on a separate
`metrics_listen` port, and ship a Grafana dashboard for the homelab.

**Architecture:** A core `metrics` module built on `prometheus-client`. A cheap,
`Clone` `Recorder` holds the event-driven families (counters, histograms) and an
in-memory `DeviceStore`; it is handed to the access-log layer, `display`, and the
pollers. A scrape-time `Collector` (owned only by the metrics listener's registry)
reads the `DeviceStore`, the dashboards' source caches, the history handle and
`/proc`. Device state is seeded once at startup from `history.db`.

**Tech Stack:** Rust 2024, axum 0.8, tokio, `prometheus-client` 0.26, `libc` 0.2
(sysconf), rusqlite, `promql-parser` 0.11 (dev only), Grafana dashboard JSON.

**Spec:** `docs/superpowers/specs/2026-11-28-prometheus-metrics-design.md`. Read
it before starting any task. It holds the full metric catalogue (names, types,
labels, values), which this plan refers to rather than repeats.

## Global Constraints

- Recording a metric never fails and never waits on I/O. `std::sync::Mutex` critical
  sections are bounded: at most O(`MAX_SAMPLES`) work (an ordered insert, or a linear
  merge of pre-sorted input), and never I/O, encoding or battery derivation. Family
  children are fetched with `get_or_create_owned` (a cloned handle), and the lookup
  lock is released before observing or awaiting. Scrape work (procfs,
  `fs::metadata`, battery derivation, encoding) runs in `tokio::task::spawn_blocking`,
  never on an async worker.
- Every new module is declared in `metrics/mod.rs` in the task that creates it, along
  with the re-exports later tasks import. A filtered `cargo test` that runs 0 tests
  counts as a failure.
- Labels never contain MACs, secrets, raw error text or kid labels. Devices are
  labelled by config `name`, and timetable sources by `timetable-kid{N}` (1-based).
- Every device-sent string goes through `metrics::labels` (allowlist plus `other`),
  on both the live path and the seeded path.
- The main router has no `/metrics`. Only the `metrics_listen` listener serves it.
- Content type: `application/openmetrics-text; version=1.0.0; charset=utf-8`.
- `prometheus-client` appends `_total` to counters: register counter families
  without the suffix (`tablica_device_resets` encodes as `tablica_device_resets_total`).
- Absent values produce no sample. Never emit 0 for "unknown".
- Seeding never increments counters.
- Commits follow Conventional Commits and carry no agent attribution (user CLAUDE.md).
- `mise run lint` and `mise run test` pass at the end of every task.

## Review Focus

1. **A device seen only through no-image 503s.** It has `last_seen` but no
   `next_checkin`; the status column must show "no reply yet", not on schedule.
   Pinned in Task 4 (`record_without_reply`) and Task 9
   (`unavailable_display_counts_as_unavailable`).
2. **A device that goes offline.** Battery gauges disappear after 7 days, but last
   seen and the info series stay. Pinned in Task 4 (`offline_device_battery_ages_out`).
3. **A seed read that finishes after live check-ins.** Live values win, and battery
   samples merge without duplicates. Pinned in Task 4 (`seed_never_overwrites_newer_live`).
4. **History disabled or `history.db` missing.** Scrape still succeeds:
   `tablica_history_enabled 0`, `db_bytes` only if the file exists, and the device
   inventory is present. Pinned in Task 10 (`scrape_with_history_disabled`).
5. **Garbage telemetry headers** (a model or firmware string with quotes or newlines,
   or 200 chars long). It maps to `other` and never reaches a label raw. Pinned in
   Task 2 (`labels_sanitise_garbage`) and Task 10 (`scrape_sanitises_garbage_labels`).

---

### Task 1: `metrics_listen` config key

**Files:**
- Modify: `crates/core/src/config.rs` (`Config`, `RawConfig`, `ConfigError`, load
  around lines 340-370, tests)
- Modify: `crates/core/src/dashboard.rs` (`RESERVED`, add `"metrics_listen"`, so
  `[_; 7]`)
- Modify: `config.sample.toml` (commented example, top level, before `[schedule]`;
  the file has no `listen` line)
- Modify (fixtures with `Config { .. }` literals): `crates/core/src/server.rs` (~199),
  `crates/core/src/device_api/mod.rs` (~506), `crates/tablica/src/render_tests.rs` (~110).
  Find any others with `grep -rn "Config {" crates`.

**Interfaces:**
- Produces: `Config.metrics_listen: Option<SocketAddr>` and the
  `ConfigError::MetricsListenClash { addr: SocketAddr }` variant, with message
  "`metrics_listen` {addr} must differ from `listen`".

- [ ] **Step 1: Write failing tests** in `config.rs` tests:
  - `metrics_listen_absent_is_none`: the existing sample config gives
    `c.metrics_listen == None`.
  - `metrics_listen_parsed`: `metrics_listen = "0.0.0.0:9101"` gives
    `Some("0.0.0.0:9101".parse().unwrap())`.
  - `metrics_listen_equal_to_listen_rejected`: `listen = metrics_listen = "0.0.0.0:8080"`
    returns `Err(ConfigError::MetricsListenClash { .. })`.
  - In `dashboard.rs`, extend `validate_names_rules`: the name `metrics_listen` is
    rejected. It is longer than 7 characters, so assert on `RESERVED.contains`
    directly.
- [ ] **Step 2: Run** `cargo test -p tablica-core config::` and expect it to FAIL to
  compile (no field).
- [ ] **Step 3: Implement.** Add `metrics_listen: Option<SocketAddr>` to the raw and
  final structs, validate it after `listen` is resolved, and add the reserved key.
  Add to `config.sample.toml`, among the top-level keys before `[schedule]`:
  `# metrics_listen = "0.0.0.0:9101"  # Prometheus /metrics; keep it off the proxied port`.
- [ ] **Step 4: Run** `cargo test --workspace` and expect PASS, with the fixtures
  listed above updated to `metrics_listen: None`.
- [ ] **Step 5: Commit** with `feat: add metrics_listen config key`.

### Task 2: Metrics module skeleton and label sanitising

**Files:**
- Modify: `Cargo.toml` (workspace deps: `prometheus-client = "0.26.0"`, `libc = "0.2"`,
  `promql-parser = "0.11.1"`)
- Modify: `crates/core/Cargo.toml` (deps `prometheus-client`, `libc`; dev-dep
  `promql-parser`)
- Create: `crates/core/src/metrics/mod.rs` (module docs, `pub mod labels;`, and the
  re-exports later tasks add)
- Create: `crates/core/src/metrics/labels.rs`
- Modify: `crates/core/src/lib.rs` (`pub mod metrics;`)
- Commit `Cargo.lock` too (the Dockerfile builds with `--locked`).

**Interfaces:**
- Produces, in `metrics::labels`:
  - `pub const DEEP_SLEEP: &str = "ESP_RST_DEEPSLEEP";`
  - `pub const RESET_REASONS: &[&str]`: the 16 names from the spec, including
    `ESP_RST_DEEPSLEEP`.
  - `pub fn reset_reason(raw: &str) -> &'static str`: the allowlisted name, else
    `"other"`.
  - `pub fn counted_reset(raw: Option<&str>) -> Option<&'static str>`: `None` when
    absent or deep sleep, else `Some(reset_reason(raw))`.
  - `pub fn counted_reset_labels() -> impl Iterator<Item = &'static str>`: every
    value `counted_reset` can return (allowlist minus deep sleep, plus `other`).
  - `pub const MODELS: &[&str] = &["og", "x", "xiao_epaper_display"];` and
    `pub fn model(raw: Option<&str>) -> Option<&'static str>`: `None` if absent,
    otherwise the allowlisted value or `"other"`.
  - `pub fn fw_version(raw: Option<&str>) -> Option<String>`: `None` if absent; the
    trimmed value if it matches `^\d{1,2}\.\d{1,2}\.\d{1,2}$`; else `"other"`.
    Hand-written check, no regex crate.

- [ ] **Step 1: Write failing tests** in `labels.rs`:
  - `reset_reasons_map`: `reset_reason("ESP_RST_BROWNOUT") == "ESP_RST_BROWNOUT"`,
    `reset_reason("garbage") == "other"`, `counted_reset(Some(DEEP_SLEEP)) == None`,
    `counted_reset(None) == None`, `counted_reset(Some("ESP_RST_SW")) == Some("ESP_RST_SW")`.
  - `counted_labels_cover_mapping`: the labels contain `ESP_RST_SW`, `ESP_RST_EXT` and
    `other`, do not contain `DEEP_SLEEP`, and number `RESET_REASONS.len()` (15 + other).
  - `labels_sanitise_garbage`: `model(Some("x\"\n"))`, `model(Some(&"a".repeat(200)))`
    and `fw_version(Some("1.2.3\nevil"))` all give `"other"`;
    `fw_version(Some("1.6.2")) == Some("1.6.2")`; `fw_version(Some("100.0.0")) == Some("other")`;
    `model(Some("xiao_epaper_display")) == Some("xiao_epaper_display")`.
- [ ] **Step 2: Run** `cargo test -p tablica-core metrics::labels` and expect FAIL.
- [ ] **Step 3: Implement** the functions above.
- [ ] **Step 4: Run** the same command and expect PASS.
- [ ] **Step 5: Commit** with `feat: add metrics label sanitising`.

### Task 3: Battery derivation on compact samples

**Files:**
- Modify: `crates/core/src/diagnostics/battery.rs`
- Modify: `crates/core/src/diagnostics/view.rs` (call site of `battery::power`)

**Interfaces:**
- Produces:
  - `pub type LatestBattery = (DateTime<Utc>, Option<f64>);`: the latest check-in's
    voltage as sent.
  - `pub fn power(latest: Option<LatestBattery>, samples: &[(DateTime<Utc>, f64)]) -> Power`:
    samples are non-zero volts, sorted by time. Same algorithm as today: 0 V on the
    latest means `NoBattery`, and the latest's voltage is added when it is not
    already among the samples.
  - `pub fn samples_from(readings: &[Reading]) -> Vec<(DateTime<Utc>, f64)>` and
    `pub fn latest_from(r: Option<&Reading>) -> Option<LatestBattery>`: adapters used
    by diagnostics and by seeding.

- [ ] **Step 1: Migrate the existing tests** to call
  `power(latest_from(..), &samples_from(..))`. Their expected values stay unchanged.
  Add `latest_zero_then_missing`: a series of positive samples with latest
  `(t, Some(0.0))` gives `NoBattery`; the same with latest `(t, None)` gives `Battery { .. }`.
- [ ] **Step 2: Run** `cargo test -p tablica-core diagnostics::battery` and expect FAIL
  (signature).
- [ ] **Step 3: Implement** the new signature. Make `view.rs` call it through the
  adapters.
- [ ] **Step 4: Run** `cargo test -p tablica-core diagnostics` and expect PASS. The
  view tests are unchanged.
- [ ] **Step 5: Commit** with `refactor: derive battery state from compact samples`.

### Task 4: `DeviceStore`

**Files:**
- Create: `crates/core/src/metrics/devices.rs`
- Modify: `crates/core/src/metrics/mod.rs` (`pub mod devices;`, re-export
  `DeviceStore`, `DeviceState`, `MetricsSeed`)

**Interfaces:**
- Consumes: `labels::{model, fw_version, counted_reset}`, `battery::LatestBattery`,
  `history::Reading`.
- Produces:
  - `pub const MAX_SAMPLES: usize = 2_500;`
  - `#[derive(Debug, Clone, Default, PartialEq)] pub struct DeviceState`, with fields
    as in the spec's "Latest vs last-known":
    - `last_seen: Option<DateTime<Utc>>`
    - `rssi: Option<(DateTime<Utc>, Option<i64>)>`
    - `last_wake: Option<(DateTime<Utc>, i64)>` (ms, non-zero only)
    - `last_image_bytes: Option<(DateTime<Utc>, i64)>`
    - `last_reply: Option<(DateTime<Utc>, i64)>` (at, next_refresh secs)
    - `last_reset: Option<DateTime<Utc>>`
    - `info: Option<(DateTime<Utc>, Option<&'static str>, Option<String>)>` (model, fw, sanitised)
    - `latest_battery: Option<LatestBattery>`
    - `samples: VecDeque<(DateTime<Utc>, f64)>`
  - `pub struct MetricsSeed`:
    - `latest`, `last_wake`, `last_image`, `last_reply`, `last_reset`, each an
      `Option<Reading>`
    - `battery: Vec<(DateTime<Utc>, f64)>` (non-zero volts in the 7-day window)

    The struct derives `Debug, Clone, Default, PartialEq`.
  - `pub struct DeviceStore` (`Default`, `Mutex<HashMap<String, DeviceState>>` keyed
    by device name) with:
    - `pub fn record(&self, device: &str, r: &Reading)`: each field merges by its own
      timestamp.
      - **Latest-only** fields (`last_seen`, `rssi`, `info`, `latest_battery`) are
        replaced only when `r.at` is newer than the stored latest. RSSI then takes
        the reading's value even when it is `None`.
      - **Last-known** fields are set when absent, or when `r.at` is newer than that
        field's own timestamp: `last_wake` (for `Some(w)` with `w != 0`),
        `last_image_bytes`, `last_reply`, and `last_reset` (when `counted_reset(..)`
        is `Some`). So an older out-of-order reading can fill a missing last-known
        field.
      - A battery voltage `Some(v)` with `v != 0` is inserted into `samples` in time
        order (binary search) and skipped if that timestamp already exists. Then the
        oldest beyond `MAX_SAMPLES` are dropped.

      Concurrent displays can finish out of order, because `at` is taken before the
      render await, so order is never assumed.
    - `pub fn seed(&self, device: &str, seed: MetricsSeed)`: applies only the
      projected field of each last-known row: `latest` drives the latest-only fields,
      `last_wake` drives only `last_wake`, and so on. Each field uses the same merge
      as `record`. `seed.battery` arrives sorted, unique and at most `MAX_SAMPLES`
      long (Task 5 caps it outside any lock). It is merged with the existing samples
      by a linear sorted merge, so the work under the lock is at most
      2 × `MAX_SAMPLES`. On an equal timestamp the existing (live) sample wins. The
      result is capped to the newest `MAX_SAMPLES`. No sorting happens under the
      lock.
    - `pub fn get(&self, device: &str) -> Option<DeviceState>` (clone).
  - `pub fn window(samples: &VecDeque<(DateTime<Utc>, f64)>, now: DateTime<Utc>) -> Vec<(DateTime<Utc>, f64)>`
    keeps samples in `(now − 7d, now]`.
  - `pub fn battery_at(state: &DeviceState, now: DateTime<Utc>) -> Power`: calls
    `battery::power` with `latest_battery` filtered to the window (a latest older
    than 7 days becomes `None`) and `window(..)`.

- [ ] **Step 1: Write failing tests:**
  - `older_reading_cannot_replace_latest_only`: newer reading (RSSI −50) then older
    (RSSI −80, model `og`) → `last_seen`, `rssi`, `info` unchanged.
  - `older_reading_fills_missing_last_known`: newer reading without wake, then older
    with wake 3000 → `last_wake == Some((t_old, 3000))`; an even older wake afterwards
    does not replace it.
  - `out_of_order_battery_inserted_sorted`: samples at t3, t1, t2, t2 → `[t1, t2, t3]`.
  - `rssi_is_latest_only`: an RSSI reading then a reading without RSSI gives
    `rssi == Some((t2, None))`.
  - `last_known_survives_missing_fields`: a reading with `wake_ms` and `image_bytes`,
    then one without either, keeps both.
  - `record_without_reply`: a reading with `next_refresh: None` leaves `last_reply == None`,
    and `last_seen` is set.
  - `samples_capped_and_sorted`: 2,600 readings give `samples.len() == 2_500`, with the
    oldest dropped.
  - `seed_never_overwrites_newer_live`: record live at t10, then seed with latest at t5
    and battery samples t1..t10 including t10. `last_seen` is t10, and samples hold
    t1..t10 once each.
  - `large_seed_merges_with_newer_live`: 2,500 live samples newer than a 2,500-sample
    seed. After `seed` there are 2,500 samples, all of them the live ones.
  - `seed_applies_projected_fields_only`: a seed whose `last_wake` row also has RSSI
    leaves `rssi` alone.
  - `offline_device_battery_ages_out`: samples and latest at `now − 8d`; `battery_at`
    gives `Power::NoData`.
  - `zero_then_missing`: latest `(t, Some(0.0))` gives `NoBattery`; a later
    `(t2, None)` gives `Battery`.
- [ ] **Step 2: Run** `cargo test -p tablica-core metrics::devices` and expect FAIL.
- [ ] **Step 3: Implement** in `devices.rs`.
- [ ] **Step 4: Run** the same command and expect PASS.
- [ ] **Step 5: Commit** with `feat: keep per-device metrics state`.

### Task 5: `History::metrics_seed`

**Files:**
- Modify: `crates/core/src/history/store.rs` (new `pub(crate) fn metrics_seed`, next to
  `snapshot`)
- Modify: `crates/core/src/history/mod.rs` (public wrapper)

**Interfaces:**
- Consumes: `metrics::devices::MetricsSeed`.
- Produces: `History::metrics_seed(&self, macs: &[String], t0: DateTime<Utc>) -> anyhow::Result<Vec<(String, MetricsSeed)>>`
  keyed by MAC, in input order. It runs in one read transaction on a read-only
  connection, with the same `READ_DEADLINE` and progress handler as `snapshot`.

  **Cutoff policy:** history stores `at` in whole seconds, so the seed reads only rows
  with `at < cutoff`, where `cutoff = t0.timestamp()` (strictly before `t0`'s
  second). Task 11 captures `t0` before any listener is bound, so every live
  check-in has `at >= t0` and never appears in the seed. A row from a previous
  process in the same second as `t0` is deliberately skipped; say so in the
  function's doc comment.

  Per MAC, with `at < cutoff` and `ORDER BY at DESC, id DESC LIMIT 1`:
  - the newest reading of any kind
  - `wake_ms IS NOT NULL AND wake_ms <> 0`
  - `image_bytes IS NOT NULL`
  - `next_refresh IS NOT NULL`
  - `reset_reason IS NOT NULL AND reset_reason <> 'ESP_RST_DEEPSLEEP'`

  Plus the battery window: `at, battery_v` of
  `at > cutoff − 7d AND at < cutoff AND battery_v IS NOT NULL AND battery_v <> 0 ORDER BY at DESC, id DESC`,
  **without** `LIMIT`. Step through the rows newest-first, skipping a row whose `at`
  equals the previous kept `at` (the first, highest-`id` row of a second wins, which
  is the last written), and stop once `MAX_SAMPLES` (2,500) distinct seconds are
  kept. Then reverse into ascending time. This all happens in the store function,
  outside any lock. `MetricsSeed.battery` is therefore sorted, unique, and holds the
  newest `min(2,500, distinct seconds)` points.
  Disabled history returns `Err`.

- [ ] **Step 1: Write failing tests** in `history/mod.rs` tests, reusing the existing
  test helpers that open a temp history and flush it:
  - `metrics_seed_reads_last_known_fields`: insert t1 (wake 5000, image 100,
    next_refresh 900, reset `ESP_RST_BROWNOUT`) and t2 (no wake, no image, no
    next_refresh, reset deep sleep). Expect `latest.at == t2`, `last_wake.at == t1`,
    `last_image.at == t1`, `last_reply.at == t1`, `last_reset.at == t1`, and a
    two-element battery list.
  - `metrics_seed_respects_cutoff`: a reading after `t0` is not returned.
  - `metrics_seed_excludes_cutoff_second`: `t0 = 12:00:00.700`; a reading at
    `12:00:00.200` (stored as second 12:00:00) is not returned; one at `11:59:59` is.
  - `delayed_seed_keeps_live_values` (store + `DeviceStore`, real history file): a
    row at t0−60s is in history and `t0` is captured. A live reading at t0+1s is
    passed to `DeviceStore::record` **and** written and flushed to SQLite through
    `History::record_reading` + flush, as production does. Then `metrics_seed` +
    `seed` gives `last_seen == t0+1s`, and the battery samples hold the t0−60s and
    t0+1s points once each (the live row is excluded from the seed by the cutoff).
  - `seed_battery_capped_and_deduped`: 3,000 distinct seconds of battery rows, plus
    one extra row duplicating the 10th-newest second with a different voltage,
    written after it. That duplicate is inside the retained range. The result has
    exactly 2,500 entries: ascending, unique, the newest 2,500 distinct seconds, with
    the duplicated second carrying the later-written voltage. Populate the fixture
    directly through `store::Store::insert_reading` on a store opened in the temp
    dir, not through the lossy 256-entry `History` queue, so no fixture row is
    dropped.
  - `metrics_seed_ties_break_by_id`: two rows with equal `at` return the later
    inserted one.
  - `metrics_seed_unavailable`: disabled history gives `Err`.
- [ ] **Step 2: Run** `cargo test -p tablica-core history::` and expect FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same command and expect PASS.
- [ ] **Step 5: Commit** with `feat: read the metrics seed from history`.

### Task 6: `Recorder` and event families

**Files:**
- Create: `crates/core/src/metrics/recorder.rs`
- Modify: `crates/core/src/metrics/mod.rs` (`mod recorder;`, re-export `Recorder`,
  `SourceMetrics`, `HttpOutcome`, `RenderKind`)

**Interfaces:**
- Consumes: `DeviceStore`, `labels`, `polling::Outcome`.
- Produces:
  - `#[derive(Clone)] pub struct Recorder(Arc<Inner>)`, re-exported as
    `metrics::Recorder`.
  - `pub fn Recorder::new(devices: &[(String /*name*/, String /*dashboard*/)]) -> Recorder`:
    pre-creates `tablica_device_resets{device,reason}` at zero for every device and
    every `labels::counted_reset_labels()`.
  - `pub fn devices(&self) -> &DeviceStore` and
    `pub fn configured(&self) -> &[(String, String)]`.
  - `pub fn record_reading(&self, device: &str, r: &Reading)`: calls
    `DeviceStore::record` and increments resets when `counted_reset` is `Some`.
  - `pub fn seed(&self, device: &str, seed: MetricsSeed)`: calls the store only; no
    counter changes.
  - `pub fn source(&self, dashboard: &str, source: &str) -> SourceMetrics`:
    pre-creates `tablica_source_fetches{dashboard,source,outcome}` for ok, failed
    and rejected, and the `tablica_source_fetch_duration_seconds{dashboard,source}`
    child. Buckets: `[0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0]`.
  - `#[derive(Clone)] pub struct SourceMetrics` with
    `pub fn observe(&self, outcome: Outcome, elapsed: Duration)`.
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)] pub enum HttpOutcome { Served, Refused, Unavailable, #[default] None }`,
    with `as_str() -> "served"|"refused"|"unavailable"|"none"`. It derives `Default`
    because `Access` derives `Default`.
  - `pub fn http(&self, route: &str, status: u16, outcome: HttpOutcome, elapsed: Duration)`:
    `tablica_http_requests{route,status,outcome}` and
    `tablica_http_request_duration_seconds{route}`. Buckets:
    `[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0]`.
  - `pub enum RenderKind { Device, Preview, Diag }`, with `as_str()`.
  - `pub fn render(&self, dashboard: &str, kind: RenderKind, elapsed: Duration, ok: bool)`:
    `tablica_render_duration_seconds{dashboard,kind}` on success,
    `tablica_render_failures{dashboard,kind}` on failure. Buckets:
    `[0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0]`.
  - `pub fn register(&self, registry: &mut Registry)`: registers the families
    above, with help texts.

- [ ] **Step 1: Write failing tests.** Encode a registry with
  `prometheus_client::encoding::text::encode` and assert on the text:
  - `resets_zero_initialised`: contains
    `tablica_device_resets_total{device="kitchen",reason="ESP_RST_SW"} 0` and no
    `ESP_RST_DEEPSLEEP`.
  - `reset_counted_once_per_checkin`: a reading with `ESP_RST_BROWNOUT` gives `1`; a
    deep-sleep reading changes nothing.
  - `seed_does_not_count`: seeding a reset row leaves the counter at 0.
  - `source_series_preinitialised`: after `source("family","weather")`, three
    outcome lines read `0` and `tablica_source_fetch_duration_seconds_count{...} 0`.
  - `http_and_render_recorded`: `http(.., Duration::from_millis(30))` gives `_sum 0.03`,
    with the `le="0.05"` bucket at 1 and `le="0.025"` at 0. `render(.., ok=false)`
    gives `tablica_render_failures_total{...} 1` and no histogram observation.
- [ ] **Step 2: Run** `cargo test -p tablica-core metrics::recorder` and expect FAIL.
- [ ] **Step 3: Implement** against prometheus-client 0.26:
  - Label sets are `#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]`
    structs.
  - Histogram families use non-capturing constructors, typed explicitly as
    `Family<L, Histogram, fn() -> Histogram>`, with one `fn` per bucket set.
  - Get children with `get_or_create_owned`, so no family lock is held while
    observing.
- [ ] **Step 4: Run** the same command and expect PASS.
- [ ] **Step 5: Commit** with `feat: add the metrics recorder`.

### Task 7: Source status contract

**Files:**
- Modify: `crates/core/src/cache.rs` (`CacheStatus`, `SourceCache::status`)
- Modify: `crates/core/src/dashboard.rs` (`SourceState`, `Dashboard::sources` default)
- Modify: `crates/family/src/dashboard.rs` (implement `sources`)

**Interfaces:**
- Produces:
  - `#[derive(Debug, Clone, Copy, PartialEq)] pub struct CacheStatus { pub fetched_at: Option<DateTime<Utc>>, pub failing: bool }`
    and `SourceCache::status(&self) -> CacheStatus`, which reads one lock and
    clones no payload.
  - `#[derive(Debug, Clone, PartialEq)] pub struct SourceState { pub id: String, pub last_success: Option<DateTime<Utc>>, pub failing: bool, pub stale: bool }`
    and `SourceState::from_status(id: impl Into<String>, s: CacheStatus, now: DateTime<Utc>, max_age: Duration) -> SourceState`.
    `stale` is true when `fetched_at` is `None` or `is_stale(..)` holds.
  - `Dashboard::sources(&self, _now: DateTime<Utc>) -> Vec<SourceState> { Vec::new() }`.
  - Family returns, in order, `calendar` (`CALENDAR_MAX_AGE`), `weather`
    (`WEATHER_MAX_AGE`), and `timetable-kid{N}` (`TIMETABLE_MAX_AGE`), for N from 1.
  - Free function `pub fn timetable_source_id(index: usize) -> String` in family
    (`format!("timetable-kid{}", index + 1)`), also used by Task 8.

- [ ] **Step 1: Write failing tests:**
  - In cache tests, `status_reflects_put_and_error`: an empty cache gives
    `{None, false}`; after `put` it gives `{Some(t), false}`; after `record_error` it
    gives `{Some(t), true}`; after another `put` it gives `{Some(t2), false}`.
  - In dashboard tests, `source_state_stale_rules`: no data gives `stale`, an old
    `fetched_at` gives `stale`, and a fresh one does not.
  - In family dashboard tests, `sources_lists_every_source_by_id`: two kids give ids
    `["calendar","weather","timetable-kid1","timetable-kid2"]`, and no kid label
    appears.
- [ ] **Step 2: Run** `cargo test --workspace` and expect FAIL (the new tests do not
  compile).
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace` and expect PASS.
- [ ] **Step 5: Commit** with `feat: expose source status for metrics`.

### Task 8: Instrument pollers

**Files:**
- Modify: `crates/core/src/polling/mod.rs` (`run_poller`)
- Modify: `crates/core/src/dashboard.rs` (`spawn_pollers` signature)
- Modify: `crates/family/src/dashboard.rs`, `crates/homelab/src/lib.rs`,
  `crates/core/src/server.rs` (test `CountingDashboard`), `crates/core/src/render/mod.rs`
  (test dashboard ~line 256), `crates/core/src/device_api/stub.rs`
- Modify: `crates/core/src/server.rs` (`run`: create the `Recorder` before spawning,
  and pass a clone to each `spawn_pollers`)

**Interfaces:**
- Consumes: `Recorder::source`, `SourceMetrics::observe`, `timetable_source_id`.
- Produces:
  - `run_poller(name: String, tz: Tz, policy: P, shutdown: CancellationToken, metrics: SourceMetrics, job: F)`.
    It times each `job()` with `Instant` and calls `metrics.observe(outcome, elapsed)`.
    `name` stays for the logs.
  - `Dashboard::spawn_pollers(self: Arc<Self>, shutdown: CancellationToken, recorder: Recorder) -> Vec<JoinHandle<()>>`.
  - Family passes `recorder.source("family", "calendar")`, `("family", "weather")` and
    `("family", &timetable_source_id(index))`.

  Task 9 moves the `Recorder` into `AppState`, built in `prepare_state`. Here, build
  it in `run` from `cfg.devices` (name, dashboard).

- [ ] **Step 1: Write failing tests.** In the polling tests, add
  `records_outcomes_and_durations`: a paused-time runner with a scripted job
  returning Ok, Failed and Rejected, each doing
  `tokio::time::sleep(Duration::from_secs(2)).await`. The clock is paused, so the
  runner must time with `tokio::time::Instant`. Expect one of each outcome,
  `_count 3`, `_sum 6`, and the `le="2.5"` bucket at 3. Update the existing polling tests to pass
  `Recorder::new(&[]).source("t","t")`.
- [ ] **Step 2: Run** `cargo test --workspace polling` and expect FAIL.
- [ ] **Step 3: Implement** and update every implementation listed above.
- [ ] **Step 4: Run** `cargo test --workspace` and expect PASS.
- [ ] **Step 5: Commit** with `feat: record poller outcomes and durations`.

### Task 9: Instrument HTTP, renders and readings

**Files:**
- Modify: `crates/core/src/device_api/mod.rs` (`AppState.recorder: Recorder`,
  `display`, `preview`, `diag_preview`, `render_blocking`)
- Modify: `crates/core/src/device_api/access_log.rs` (`Access.outcome`, the layer
  records)
- Modify: `crates/core/src/server.rs` (`prepare_state` builds the `Recorder` and puts
  it in `AppState`; `run` takes it from the state, replacing Task 8's local)
- Modify (fixtures with `AppState { .. }` literals): `crates/core/src/device_api/mod.rs`
  test fixture (~536), `crates/tablica/src/render_tests.rs` (~127); find others with
  `grep -rn "AppState {" crates`.

**Interfaces:**
- Consumes: `Recorder::{http, render, record_reading}`, `HttpOutcome`, `RenderKind`.
- Produces:
  - The `Access` fields gain `pub outcome: HttpOutcome`. `served` maps to `Served`,
    `refused` to `Refused`, and new `Access::unavailable(device: &str, reason: impl Into<String>)`
    to `Unavailable`; the no-image 503 uses it. A response without a note maps to
    `HttpOutcome::None`.
  - The layer reads `axum::extract::MatchedPath` from the request extensions before
    `next.run` (`"unmatched"` when absent), and calls
    `state.recorder.http(route, status, outcome, elapsed)`.
  - `render_blocking(state, name, now, depth, kind: RenderKind)` records the render.
    `diag_preview` records with the device's dashboard and `RenderKind::Diag`.
  - `display` calls `state.recorder.record_reading(&device.name, &reading)` at each
    point where it calls `state.history.record_reading(reading)` (before the move).

- [ ] **Step 1: Write failing router tests** in `device_api/mod.rs` tests, using the
  existing fixture helpers and a helper that encodes a `Registry` from
  `state.recorder`:
  - `display_records_request_and_device`: one display gives
    `tablica_http_requests_total{route="/api/display",status="200",outcome="served"} 1`
    and the device's `last_seen` is set in `recorder.devices()`.
  - `unknown_token_counts_as_refused`.
  - `unavailable_display_counts_as_unavailable`: the render fails with no previous
    image (the existing 503 test setup) and gives `outcome="unavailable"`, no
    `refused` line, and a device state with `last_reply == None`.
  - `unmatched_route_label`: GET `/nope` gives `route="unmatched",status="404",outcome="none"`.
  - `main_router_has_no_metrics`: GET `/metrics` on `router(state)` gives 404.
- [ ] **Step 2: Run** `cargo test -p tablica-core device_api` and expect FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace` and expect PASS.
- [ ] **Step 5: Commit** with `feat: record HTTP, render and device metrics`.

### Task 10: Scrape-time collector, process metrics, encoding

**Files:**
- Create: `crates/core/src/metrics/collector.rs`
- Create: `crates/core/src/metrics/process.rs`
- Modify: `crates/core/src/metrics/mod.rs` (`mod collector; pub mod process;`,
  `build_registry`, `encode`, re-export `ScrapeContext`)

**Interfaces:**
- Consumes: `Recorder`, `DeviceStore`, `devices::battery_at`, `Dashboard::sources`,
  `History::{is_available, dropped}`, `Clock`.
- Produces:
  - `pub struct ScrapeContext { pub recorder: Recorder, pub dashboards: Arc<Active>, pub history: Arc<History>, pub data_dir: PathBuf, pub clock: Clock }`
  - `pub fn build_registry(ctx: ScrapeContext) -> Registry`: registers the recorder's
    families, a `Collector` impl for every scrape-time metric in the spec (device
    inventory and state, source gauges, history, build info), and the process
    collector.
  - `pub fn encode(registry: &Registry) -> Result<String, std::fmt::Error>`, via
    `prometheus_client::encoding::text::encode` (one `# EOF`). An error fails the
    scrape with a 500; never send a partial body.
  - Collector mechanics (0.26):
    - The collector struct holds the `ScrapeContext` and gets a manual `impl Debug`,
      because `Arc<Active>` and `Clock` are not `Debug`. It must be
      `Send + Sync + 'static`.
    - In `Collector::encode(&self, mut enc: DescriptorEncoder)`, call
      `enc.encode_descriptor(name, help, unit, MetricType::…)` once per family, then
      `encode_family(&labels)` per label set with `ConstGauge`.
    - Counters (`tablica_history_dropped`, `process_cpu_seconds`) use `ConstCounter`
      and a descriptor name without `_total`.
  - `pub const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";`
  - In `process.rs`:
    - `pub struct ProcSample { cpu_seconds: Option<f64>, rss_bytes: Option<f64>, open_fds: Option<f64>, start_time: Option<f64> }`
    - `pub fn parse(stat: &str, statm: &str, proc_stat: &str, clk_tck: f64, page_size: f64) -> ProcSample`
      is pure. `/proc/self/stat` fields 14+15 are utime and stime, and field 22 is
      starttime. Parse after the last `)`. `btime` comes from `/proc/stat`. Each
      field is independent; a parse failure gives `None`.
    - `#[cfg(target_os = "linux")] pub fn read() -> ProcSample` uses
      `libc::sysconf(_SC_CLK_TCK)` and `_SC_PAGESIZE`, plus a `read_dir` count of
      `/proc/self/fd`.
    - Non-Linux `read()` returns all `None`.
  - Device gauges: `next_checkin = last_reply.at + next_refresh`, and
    `wake_seconds = last_wake.ms / 1000.0`. Battery gauges come from
    `battery_at(state, now)` and are emitted only for `Power::Battery`; `days_left`
    is emitted only for `Estimate::DaysLeft`. `tablica_device_info` is emitted only
    when `info` is set, with `model` and `fw_version` labels, absent ones encoded as
    `""`. `tablica_build_info{version=env!("CARGO_PKG_VERSION")}`.
  - `history_db_bytes`: `std::fs::metadata(data_dir.join("history.db")).len()`,
    absent on error.

- [ ] **Step 1: Write failing tests:**
  - In `process.rs`, `parse_fixture`: a fixture `stat` line whose comm contains
    spaces and `)`, with `clk_tck` 100, `page_size` 4096 and `btime` 1700000000.
    Expect `cpu_seconds == (utime+stime)/100`, `start_time == 1700000000 + starttime/100`,
    and `rss == pages*4096`. `parse_garbage` gives all `None`.
  - In `collector.rs`, using `StubDashboard` and a temp history:
    - `scrape_contains_inventory_for_never_seen`: two configured devices with no
      readings give two `tablica_device_configured` lines and no
      `tablica_device_last_seen_timestamp_seconds`.
    - `scrape_device_state`: after `record_reading` with battery 4.0 V, RSSI −60,
      wake 4000, image 1234 and next_refresh 900, expect
      `tablica_device_battery_volts`, `_percent`, `wifi_rssi_dbm -60`,
      `wake_seconds 4`, `image_bytes 1234`, and `next_checkin == at+900`.
    - `scrape_with_history_disabled`, two cases:
      - A `data_dir` without `history.db` and a disabled handle gives
        `tablica_history_enabled 0`, no `tablica_history_db_bytes`, and the
        inventory is present.
      - An existing `history.db` file with a disabled handle gives
        `tablica_history_enabled 0`, and `tablica_history_db_bytes` equal to the
        file's size.
    - `scrape_sources`: a test dashboard whose `sources()` returns one failing,
      stale source gives `tablica_source_failing{...} 1` and `tablica_source_stale{...} 1`.
    - `scrape_sanitises_garbage_labels`: record a reading with model `x"\n` and fw
      `1.2.3\nevil`. Expect the exact sample
      `tablica_device_info{device="kitchen",model="other",fw_version="other"} 1`, no
      line containing `evil` or a raw newline inside braces, and
      `body.trim_end().ends_with("# EOF")`. This makes no claim of full OpenMetrics
      validation: the encoder belongs to the library, and the test pins only our
      label values.
- [ ] **Step 2: Run** `cargo test -p tablica-core metrics::` and expect FAIL.
- [ ] **Step 3: Implement.** The collector calls `DeviceStore::get` per configured
  device (a clone under the lock), and derives battery outside it. Callers of
  `encode` (Task 11's handler) run it inside `spawn_blocking`.
- [ ] **Step 4: Run** the same command and expect PASS.
- [ ] **Step 5: Commit** with `feat: collect device, source and process metrics at scrape time`.

### Task 11: Metrics listener, shutdown and seeding

**Files:**
- Modify: `crates/core/src/server.rs`
- Modify: `crates/core/src/metrics/mod.rs` (`pub fn router(registry: Arc<Registry>) -> Router`)
- Modify: `crates/core/Cargo.toml` (dev-dependency `tokio` features: add `"io-util"` to
  the existing `["test-util", "macros", "rt"]`, for `AsyncReadExt`/`AsyncWriteExt` in
  the raw TCP test helper; commit `Cargo.lock` if it changes)

**Interfaces:**
- Consumes: `build_registry`, `encode`, `CONTENT_TYPE`, `History::metrics_seed`,
  `Recorder::seed`, `Config.metrics_listen`.
- Produces:
  - `metrics::router(registry)`: `GET /metrics` returns 200 with `CONTENT_TYPE` and
    a body from `encode(..)` run in `spawn_blocking` (500 on an encode error). Every
    other path returns 404.
  - `fn seed_metrics(history: &History, recorder: &Recorder, devices: &[Device], t0: DateTime<Utc>)`:
    calls `history.metrics_seed(macs, t0)`, then `recorder.seed(name, seed)` per
    device mapped MAC→name. On error, `warn!("metrics seed unavailable: {e:#}")`.
  - The public `ServeOptions` and `run` signature stay unchanged, so `main.rs` and the
    test helper literals keep compiling. `run` delegates to a private
    `async fn run_with(loaded, opts, shutdown, on_bound: Option<tokio::sync::oneshot::Sender<(SocketAddr, Option<SocketAddr>)>>)`,
    passing `None`. `run_with` sends the bound addresses right after both binds, and
    tests call it directly.
  - `server::run` flow:
    1. `prepare_state`.
    2. Install `shutdown()`.
    3. **Capture `t0 = (state.clock)()`.**
    4. Bind `listen`, then bind `metrics_listen` if set. Either failure returns `Err`
       before any poller or seed starts.
    5. Start the seed thread (`std::thread::Builder` named `metrics-seed`) running
       `seed_metrics(.., t0)`, then spawn pollers with `recorder.clone()`.
    6. A task awaits the shutdown future once, then cancels `stop: CancellationToken`.
    7. Both listeners' `axum::serve(..).with_graceful_shutdown(stop.clone().cancelled_owned())`
       are driven with `tokio::join!`.
    8. Wait for the pollers (the existing timeout), then flush history.

    Keep the existing doc comments accurate.

- [ ] **Step 1: Write failing tests** in `server.rs` tests, following the existing
  `run` tests. Use **real TCP listeners** through `run_with`: bind `127.0.0.1:0`, take
  the addresses from `on_bound`, and send requests over a raw `tokio::net::TcpStream`
  (HTTP/1.1, `Connection: close`). Wrap the readiness wait, each request and the
  final `run_with` join in `tokio::time::timeout(Duration::from_secs(5), ..)`, so a
  broken readiness or shutdown path fails instead of hanging.
  - `both_listeners_serve_then_stop`:
    1. Before `run_with`, pre-write a reading for the configured device, stamped
       `Utc::now() - 10s` (at least one whole second before startup, so the
       strict-second cutoff cannot exclude it), into `history.db` via
       `History::open` + `record_reading` + `shutdown`.
    2. Wait for `on_bound`. Poll `GET /metrics` on the metrics port (up to 2 s)
       until it contains `tablica_device_last_seen_timestamp_seconds{device="kitchen"}`
       with the seeded value, before any live check-in. Check the content type and
       `# EOF`.
    3. `GET /healthz` on the metrics port gives 404, `GET /metrics` on the main port
       gives 404, and `GET /healthz` on the main port gives 200.
    4. Resolve the shutdown future. `run` returns `Ok` within the stop timeout, and
       new connections to both ports are refused.
  - `second_bind_failure_starts_no_pollers`: `metrics_listen` set to an address
    already bound by the test gives `Err`, and the `Counting` poller count is 0.
  - `no_metrics_listen_binds_only_main`: with `metrics_listen: None`, `on_bound`
    yields `(main, None)`, `GET /healthz` on main gives 200, `GET /metrics` on main
    gives 404, and after the shutdown signal `run_with` returns `Ok`.
  - `seed_metrics_fields` (synchronous, field-level): `seed_metrics` against a temp
    history with wake, image, reply and reset rows sets each field in the
    recorder's state, and the resets counter stays 0.
- [ ] **Step 2: Run** `cargo test -p tablica-core server::` and expect FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace` and expect PASS.
- [ ] **Step 5: Commit** with `feat: serve /metrics on metrics_listen`.

### Task 12: Grafana dashboard and its test

**Files:**
- Create: `contrib/grafana/tablica.json`
- Create: `crates/core/tests/grafana_dashboard.rs` (integration test)

**Interfaces:**
- Consumes: the spec's "Grafana dashboard" section (rows, panels, queries,
  thresholds, no-data messages), `metrics::{build_registry, encode}`, and `Recorder`.
- Produces: the dashboard JSON:
  - `uid` `tablica`, title `tablica`, `schemaVersion` 39 or higher.
  - Datasource `{"type":"prometheus","uid":"${datasource}"}` is not used. Every
    panel and target uses `"datasource": "Prometheus"`, as the other provisioned
    dashboards do.
  - Variable `device`: query `label_values(tablica_device_configured{job="tablica"}, device)`,
    multi, include all.
  - Every `expr` contains `job="tablica"`.

- [ ] **Step 1: Write the failing test** `dashboard_queries_are_valid_and_known`:
  - Load the JSON and walk `panels[*].targets[*].expr` (including nested row
    panels) and `templating.list[*].query`. Substitute `$device` to `.*`,
    `${device}` to `.*`, `$__range` to `1d`, `$__rate_interval` to `5m`, and
    `$__interval` to `1m`.
  - For `label_values(<sel>, <label>)`, parse `<sel>`. Parse every expr with
    `promql_parser::parser::parse` and assert `Ok`.
  - Collect metric names by walking the AST. Handle `VectorSelector` and
    `MatrixSelector` (via its `.vs`, since promql-parser 0.11.1 treats
    `MatrixSelector` as a leaf), plus function args, aggregations, binary ops,
    parens and subqueries.
  - Assert each name is in the encoded scrape of a registry built with sample state:
    - one device with full telemetry, one battery-less (0 V), one never seen;
    - a test dashboard with one failing source;
    - history enabled;
    - every lazy family instantiated: record one HTTP request, one successful
      render, one failed render and one poller observation.
  - Histogram names may appear as `_bucket`, `_sum` or `_count`. Skip names starting
    with `process_` unless `cfg!(target_os = "linux")`.
  - Assert that every panel's **and every target's** `datasource` is `"Prometheus"`.
    It may be absent on row panels only.
  - Add a unit case in the same file, `range_selector_typo_is_caught`: the name
    extractor applied to `increase(tablica_device_resetz_total[1d])` yields
    `tablica_device_resetz_total`, which the known-name check rejects.
  - The main test fails at first because the JSON is missing.
- [ ] **Step 2: Run** `cargo test -p tablica-core --test grafana_dashboard` and expect
  FAIL.
- [ ] **Step 3: Write `contrib/grafana/tablica.json`** per the spec:
  - **Devices row:** status table outer-joined on `device`, with the status value
    mappings:
    - never seen
    - no reply yet
    - on schedule: ≤ 300 s past the deadline
    - late: ≤ 1800 s
    - overdue

    Then the battery, RSSI and wake time series, and the resets table (no-data
    "No resets").
  - **Sources row:** status table, fetches by outcome, and p95 over `[1d]` (no-data
    "No recent fetches").
  - **Server row:** request rate, problem requests (`outcome=~"refused|unavailable"`,
    no-data "None"), render p50/p95, render failures, history size and drops,
    process RSS and CPU, and version.
- [ ] **Step 4: Run** the same command and expect PASS. Also run
  `python3 -m json.tool contrib/grafana/tablica.json > /dev/null`.
- [ ] **Step 5: Commit** with `feat: add the Grafana dashboard for tablica`.

### Task 13: Docs and homelab wiring

**Files:**
- Modify: `README.md` (a "Metrics" subsection under Diagnostics: `metrics_listen`,
  the trust boundary in the spec's words, the metric groups, the first-scrape
  caveat, and `contrib/grafana/tablica.json`)
- Modify: `contrib/docker/docker-compose.yaml` (a comment that `metrics_listen` is
  not published)
- Homelab repo `~/Projects/homelab/homelab`, left **uncommitted**:
  - `tablica/docker-compose.yaml`: add `networks: [default, monitoring]` to the
    service, and top-level `networks: {default: {}, monitoring: {external: true, name: monitoring}}`.
    No port for 9101.
  - `tablica/SETUP.md`: the config step adds `metrics_listen = "0.0.0.0:9101"`. Add
    a Metrics section covering:
    - the grafana stack must be up first (it creates `monitoring`);
    - tablica needs an image version with metrics, and `docker-compose up -d`
      recreates the container on the new network;
    - after changing `prometheus.yaml`, run `docker restart prometheus`;
    - the manual verification checklist from the spec's Testing section.
  - `grafana/config/prometheus.yaml`: job `tablica`, target `tablica:9101`.
  - `grafana/provisioning/dashboards/tablica.json`: a copy of
    `contrib/grafana/tablica.json`.

- [ ] **Step 1: Write the docs and homelab changes.**
- [ ] **Step 2: Verify.** If `docker` is not available, say so and skip the
  container checks.
  - Compose: `tablica/`'s service declares `env_file: .env`, so `--env-file` alone
    fails. For each of `tablica/` and `grafana/`, copy the project directory to the
    session scratchpad. If the real `.env` is absent, write a fixture `.env` there
    from `env.sample`, with placeholder values replaced by plausible ones (a version
    `0.0.0`, paths under the scratch dir). Then run `docker-compose config -q` in the
    copy, using the homelab's CLI. Never write a `.env` into the homelab repo.
  - Prometheus config: run
    `docker run --rm --entrypoint promtool -v ~/Projects/homelab/homelab/grafana/config:/c:ro prom/prometheus:<PROMETHEUS_VERSION from grafana/.env or env.sample> check config /c/prometheus.yaml`
    and expect `SUCCESS`.
  - Run
    `diff contrib/grafana/tablica.json ~/Projects/homelab/homelab/grafana/provisioning/dashboards/tablica.json`
    and expect no output.
- [ ] **Step 3: Run** `mise run lint && mise run test` and expect PASS.
- [ ] **Step 4: Commit** (tablica repo only) with `docs: describe metrics and the Grafana dashboard`.
