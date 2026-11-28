# Prometheus metrics and Grafana dashboard

## Intent

Expose tablica's own health and the state of its TRMNL devices to the homelab
Prometheus, with a provisioned Grafana dashboard, so a dying battery, a device that
stopped checking in, a failing source (Librus, calendar, weather) or a broken render
shows up on a graph and can be alerted on, instead of only on the per-device
diagnostics screen.

## Goals

- Prometheus/OpenMetrics text at `GET /metrics` on a **separate, optional listener**
  (`metrics_listen`), never on the main device/proxy listener.
- Every metric group the user selected (all of them, below).
- Device gauges survive a restart: seeded from `history.db` at startup.
- A Grafana dashboard JSON, kept in this repo, provisioned in the homelab Grafana.
- Homelab wiring: tablica joins the `monitoring` network; Prometheus scrapes the
  metrics port there.

## Non-goals

- Alert rules (the dashboard and metrics make them possible; writing them is later).
- Auth on `/metrics`: the boundary is the network (below).
- Metrics for the homelab placeholder dashboard beyond what core gives every dashboard.

## Anti-goals

- Recording a metric must never fail, block or delay a device reply or a poller
  (same rule as the history).
- No secrets, raw error text, MAC addresses or free-form display text (kid labels) in
  labels or values. Devices are labelled by their config `name`.
- No unbounded label cardinality and no unbounded in-memory buffers from
  device-sent data.

## Approaches

- **A. `prometheus-client`, explicit handles, scrape-time collectors (chosen).**
  Counters/histograms are updated where events happen; state-like gauges are
  computed at scrape time. Fits the codebase's explicit dependency passing (clock,
  history) and keeps gauges consistent with the diagnostics screen by sharing its
  derive functions.
- B. `metrics` facade + `metrics-exporter-prometheus`: global recorder, least wiring,
  but global state complicates tests and gauges must be pushed rather than read.
- C. tikv `prometheus`: mature, built-in process collector, but older macro-heavy API
  and nothing A lacks.

The user picked A.

## Trust boundary

The main listener is proxied by DSM for the whole `tablica.home…` hostname, so
anything on it is reachable from the LAN and tailnet (and directly when
`TABLICA_BIND=0.0.0.0`). Therefore:

- New optional top-level config key `metrics_listen` (socket address, e.g.
  `"0.0.0.0:9101"`). Absent: no metrics listener at all (the in-process recording
  still happens, cheaply). It must differ from `listen`; config validation rejects
  equal addresses.
- The metrics listener serves only `GET /metrics` (404 elsewhere). The main router
  has no `/metrics`.
- In the container it binds `0.0.0.0:9101`, the port is **not published** in compose
  and not proxied. Real boundary, documented as such: reachable by containers on the
  networks tablica joins (its own compose `default`, which holds only tablica, and
  `monitoring`, which holds Prometheus, Grafana, Loki, unpoller, immich) and by the
  Docker host itself; not by the LAN or tailnet. Exclusive-to-Prometheus isolation is
  a non-goal. The metrics expose device names, model and firmware, battery, RSSI and
  check-in schedule, source health under opaque ids, request and render counts, and
  process stats; anyone who can reach the port can read them.
- Joining `monitoring` also makes the main API on 8080 reachable by peers on that
  network. Accepted, not isolated: everything it serves is already reachable from the
  LAN and tailnet through the DSM proxy.
- `metrics_listen` is added to core's reserved top-level keys.
- Shutdown with two listeners: `server::run` awaits the caller's shutdown future once,
  in a task that cancels a `CancellationToken`; both `axum::serve` calls use
  `token.cancelled()` as their graceful-shutdown future, and are driven together
  (`tokio::join!`). Pollers are cancelled from the same token. History is flushed only
  after both servers have drained. Both listeners are bound before any poller starts;
  a failure binding either is a startup error with no pollers spawned (tested for the
  second bind).

## Design

### Crate and module layout

- Add `prometheus-client` to the workspace and to `tablica-core`.
- New module `crates/core/src/metrics/`:
  - `Recorder` (cheap, `Clone`, `Arc` inside): the event-driven families only
    (counters/histograms) plus the device state store. It holds **no** references to
    dashboards, history or caches. This is what `AppState`, the access-log layer,
    `display` and pollers get.
  - `Collector` (scrape-time): built by `server.rs` with the `Recorder`, the
    `Arc<Active>` dashboards, the `Arc<History>`, `data_dir` and the clock. Registered
    into the `Registry`; the `Registry` is owned by the metrics listener's state only.
    No dashboard holds the collector or registry, so there is no `Arc` cycle.
- Dashboards do **not** get a metrics handle at `prepare`. The trait becomes
  `Dashboard::spawn_pollers(self: Arc<Self>, shutdown: CancellationToken, recorder: Recorder)`
  (owned, cheap `Clone`), passed by `server::run`; `run_poller` takes a
  `PollerMetrics` (recorder + dashboard + source id) per poller. Every implementation
  is updated: family, homelab (`crates/homelab/src/lib.rs`), and the core test
  dashboards (`server.rs` `CountingDashboard`, `render/mod.rs`, `device_api/stub.rs`).
- Content type `application/openmetrics-text; version=1.0.0; charset=utf-8`.

### Label sanitising (`metrics/labels.rs`)

All device-sent strings go through a finite mapping, on both the live and seeded paths:

- `reset_reason`: allowlist of the ESP-IDF `esp_reset_reason_t` names
  (`ESP_RST_UNKNOWN, POWERON, EXT, SW, PANIC, INT_WDT, TASK_WDT, WDT, DEEPSLEEP,
  BROWNOUT, SDIO, USB, JTAG, EFUSE, PWR_GLITCH, CPU_LOCKUP`), otherwise `other`.
- `model`: allowlist of known TRMNL firmware model names (builder: at least `og`,
  `x`, and `xiao_epaper_display` from the existing test fixture in
  `device_api/mod.rs`; anything else `other`). One info series per device at a time.
- `fw_version`: kept only if it matches `^\d{1,2}\.\d{1,2}\.\d{1,2}$`, else `other`.
  Call: only authenticated devices (a handful, in one household) can send it and
  firmware changes a few times a year, so a bounded semver pattern is accepted
  instead of a hardcoded version list that would go stale; total series over
  retention stay in the tens.

`Recorder` initialisation and ingestion use the same mapping functions, so the
zero-initialised series set is derived from the mapping, not listed by hand.

### Metric catalogue

Label `device` is the config device name; `dashboard` the dashboard entry name.

**Device inventory and state** (scrape-time):

| Metric | Type | Labels | Value |
|---|---|---|---|
| `tablica_device_configured` | gauge (=1) | device, dashboard | one per configured device, always present |
| `tablica_device_last_seen_timestamp_seconds` | gauge | device | newest reading `at`; absent if never seen |
| `tablica_device_next_checkin_timestamp_seconds` | gauge | device | last successful reply's `at + next_refresh` (the deadline it was given) |
| `tablica_device_battery_volts` | gauge | device | `battery::power` median volts |
| `tablica_device_battery_percent` | gauge | device | `battery::power` |
| `tablica_device_battery_days_left` | gauge | device | `Estimate::DaysLeft` only |
| `tablica_device_wifi_rssi_dbm` | gauge | device | latest reading's RSSI (latest-only) |
| `tablica_device_wake_seconds` | gauge | device | last-known non-zero `wake_ms` / 1000 |
| `tablica_device_image_bytes` | gauge | device | last-known rendered image size |
| `tablica_device_last_reset_timestamp_seconds` | gauge | device | newest reading whose reason is present and not `ESP_RST_DEEPSLEEP` |
| `tablica_device_info` | gauge (=1) | device, model, fw_version | latest reading, sanitised |

Absent values produce no sample. A 0 V battery reading means "no battery": the
battery gauges are absent.

**Latest vs last-known.** Per device the store keeps, each with its own timestamp:
`last_seen` (any reading), `rssi` (latest reading only: a missing RSSI clears it),
`last_wake` (newest reading with non-zero `wake_ms`), `last_image_bytes` (newest
reading with `image_bytes`), `last_reply` (`at` and `next_refresh` of the newest
reading with `next_refresh`, i.e. a successful reply), `last_reset` (`at`
only), `info` (latest). Updates merge by timestamp: a field is replaced
only by a newer value.

**Battery samples.** Per device:
- `latest_battery: Option<(DateTime<Utc>, Option<f64>)>`: the latest check-in's
  voltage as sent, including `Some(0.0)` (no battery) and `None` (omitted), so
  positive → 0 → missing behaves like diagnostics (`power` looks at the latest
  reading's 0 V).
- `samples: VecDeque<(DateTime<Utc>, f64)>` of non-zero volts, kept sorted by time,
  capped at the newest 2,500 by time (an older seed arriving after live samples is
  merged in order and the oldest are dropped), deduplicated by timestamp.

At scrape time the collector copies both under a short lock, filters samples to
`(now − 7d, now]`, and runs battery derivation outside the lock. `battery::power`
is refactored to take `(latest: Option<(DateTime<Utc>, Option<f64>)>, samples: &[(DateTime<Utc>, f64)])`
(diagnostics adapts its `Reading`s to that), so both screens share one function.
Deliberate difference from diagnostics, which also counts the latest voltage when it
is older than 7 days: metrics drop it once it leaves the window, so an offline device
loses its battery gauges after 7 days instead of showing a frozen value; last-seen
already shows it is offline. Tested: positive → 0 → missing, and an offline device
with a lone old latest voltage.

**Device events**:

| Metric | Type | Labels |
|---|---|---|
| `tablica_device_resets_total` | counter | device, reason |

Incremented on each check-in whose reason is present and not `ESP_RST_DEEPSLEEP`.
Series for every configured device × every value the reset mapping can produce
except `ESP_RST_DEEPSLEEP` (allowlist + `other`) are created at zero at startup, so
`increase()` sees a reset that happens after the first scrape. A reset recorded
before Prometheus's first scrape of a fresh process can still be missed (documented
limitation). Not seeded from history: seeding never emits events.

**Sources** (every poller run, recorded by `run_poller`):

| Metric | Type | Labels |
|---|---|---|
| `tablica_source_fetches_total` | counter | dashboard, source, outcome=`ok\|failed\|rejected` |
| `tablica_source_fetch_duration_seconds` | histogram | dashboard, source |

Buckets: 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, 60 s. Outcome counter children and the
duration histogram child are pre-created at zero for each poller when it starts
(same first-scrape caveat as resets).

**Sources** (scrape-time):

| Metric | Type | Labels |
|---|---|---|
| `tablica_source_last_success_timestamp_seconds` | gauge | dashboard, source |
| `tablica_source_failing` | gauge 0/1 | dashboard, source |
| `tablica_source_stale` | gauge 0/1 | dashboard, source |

Source IDs are opaque and identical for counters and gauges: `calendar`, `weather`,
`timetable-kid1`, `timetable-kid2`, … (1-based config order, same as the cache file
names). Kid labels never appear.

`SourceCache` gets a `status(&self) -> CacheStatus { fetched_at: Option<DateTime<Utc>>, failing: bool }`
read under one read lock without cloning the payload. `failing` = an error is
recorded (a success clears it). `stale` = no data or age above the source's
`max_age` (same rule as the diagnostics row).

New default method on `Dashboard`:

```rust
fn sources(&self, now: DateTime<Utc>) -> Vec<SourceState> { Vec::new() }
```

`SourceState { id: &'static str or String, last_success, failing, stale }`, with a core
helper from `CacheStatus` + `max_age`. Family implements it with the max ages it
already uses for diagnostics.

**Server** (event-driven):

| Metric | Type | Labels |
|---|---|---|
| `tablica_http_requests_total` | counter | route, status, outcome |
| `tablica_http_request_duration_seconds` | histogram | route |
| `tablica_render_duration_seconds` | histogram | dashboard, kind=`device\|preview\|diag` |
| `tablica_render_failures_total` | counter | dashboard, kind |

`route` is axum's `MatchedPath` or `unmatched`. `outcome` is a new typed enum carried
in the `Access` note: `served`, `refused` (unknown token / unknown MAC / unauthorised
log post), `unavailable` (the no-image 503), `none` (no note: images, previews,
healthz, 404s). The access-log layer keeps using the same note for its text and maps
the enum to the label; the 503 path is tested so it never counts as `refused`.

The problem series are pre-created at zero at startup (same first-scrape caveat as
resets), so `increase()` sees the first refused or unavailable request and the first
render failure: every `(route, status, outcome)` the handlers can return as refused or
unavailable (`/api/setup` 404 refused, `/api/display` 401 refused, `/api/display` 503
unavailable, `/api/log` 401 refused; the other routes never refuse), and
`tablica_render_failures_total` for every active dashboard × kind. The list lives next
to the recorder and must follow the handlers.

**History store** (scrape-time):

| Metric | Type |
|---|---|
| `tablica_history_enabled` | gauge 0/1 |
| `tablica_history_db_bytes` | gauge (file size of `data_dir/history.db`, absent if missing) |
| `tablica_history_dropped_total` | counter (existing `History::dropped()`: queue/budget drops only, not "history disabled" drops) |

**Build and process** (scrape-time):

- `tablica_build_info{version}` = 1.
- Linux only, each sample independent (a failing read omits only that sample):
  - `process_cpu_seconds_total`: `(utime + stime) / CLK_TCK` from `/proc/self/stat`.
  - `process_resident_memory_bytes`: `statm` RSS pages × page size.
  - `process_open_fds`: entries in `/proc/self/fd`.
  - `process_start_time_seconds`: `btime` from `/proc/stat` + `starttime / CLK_TCK`.
  - `CLK_TCK` and page size via `libc::sysconf` (add `libc`), or the builder may use
    the `procfs` crate if it is lighter to integrate.

### Seeding

`server::run` captures `t0 = now` before binding either listener, then starts a
detached thread (like `read_history`) that calls a new
`History::metrics_seed(macs, t0)`. History stores `at` in whole seconds, so the seed
reads only rows strictly before `t0`'s second (`cutoff = t0.timestamp()`, `at < cutoff`):
every live check-in is at or after `t0` and is never also seeded; a previous
process's row in that same second is deliberately skipped. It reads, in one read
transaction, per device:
- the newest reading (any), the newest with non-zero `wake_ms`, the newest with
  `image_bytes`, the newest with `next_refresh`, the newest with a present
  non-deep-sleep `reset_reason`;
- the newest 2,500 non-zero battery volts of `(cutoff − 7d, cutoff)`, one per stored
  second (the last-written row wins), returned in ascending time.
Each "newest" orders by `(at DESC, id DESC)` so SQL ties are deterministic.

Hydration is a separate, event-free path (`DeviceStore::seed`): it applies only the
projected field from each last-known row (merge by timestamp, never overwriting a
newer live value), builds battery samples once from the window (merged in time order
with any live samples, deduplicated by timestamp), and never increments counters.
History disabled or a failed read: logged once, store stays empty;
`tablica_device_configured` still lists the devices.

### Grafana dashboard

`contrib/grafana/tablica.json` (source of truth; datasource by name `Prometheus` like
the other provisioned dashboards), copied to
`homelab/grafana/provisioning/dashboards/tablica.json`. Variable `device` (multi, all)
from `label_values(tablica_device_configured{job="tablica"}, device)`. All queries
scoped to `job="tablica"`.

- **Devices row:**
  - Status table, one row per configured device: the inventory series
    `tablica_device_configured` outer-joined (Grafana "Join by field" on `device`)
    with the telemetry queries, so devices with missing telemetry still get a row.
    Columns: since last seen (`time() - last_seen`, "never" when absent); a **status**
    column derived from the deadline: `never seen` (no last_seen), `no reply yet`
    (last_seen but no next_checkin: only no-image 503s), `on schedule` (now ≤ deadline
    + 5 min, which includes scheduled overnight sleep), `late` (≤ 30 min past), `overdue`
    (beyond); battery %, days left, RSSI, firmware.
  - Time series: battery %, RSSI, wake seconds.
  - Resets in range table (`increase(tablica_device_resets_total[$__range]) > 0`),
    expected empty when healthy: Grafana's built-in "No data" means no reset occurred
    in the range (no custom no-data text, which would also cover a missing scrape).
- **Sources row:** status table (last success age, failing, stale), fetches by outcome
  (`increase(...[$__range])`), p95 fetch duration over `[1d]` windows (covers the
  overnight gap and jittered 5 h daytime intervals), with a "no recent fetches"
  no-data message.
- **Server row:** request rate by route/status, problem requests
  (`outcome=~"refused|unavailable"`, `> 0`, expected empty when healthy: "No data"
  means none occurred, as for resets), render p50/p95 by dashboard and kind, render
  failures (a plain `sum(increase(...))` with no `or vector(0)` fallback: the series
  are pre-created at zero, so 0 means no failure and "No data" means no scrape data),
  history DB size and drops, RSS/CPU (`process_*{job="tablica"}`), version.

### Homelab wiring (separate repo, `~/Projects/homelab/homelab`)

- `tablica/docker-compose.yaml`: join the external `monitoring` network (as unpoller
  does), keep `default`; do not publish 9101. Prerequisite documented in
  `tablica/SETUP.md`: the `monitoring` network is created by the grafana stack, so it
  must be up first.
- `tablica/SETUP.md`: add `metrics_listen = "0.0.0.0:9101"` to the config step and a
  short Metrics section.
- `grafana/config/prometheus.yaml`: job `tablica`, target `tablica:9101`. Prometheus
  has no lifecycle reload flag here, so apply with `docker restart prometheus` (or
  `docker kill -s HUP prometheus`); documented.
- Add the dashboard JSON.
- Changes there are left uncommitted for the user to review.

### Docs

README: a "Metrics" subsection under Diagnostics: `metrics_listen`, the trust
boundary, the metric groups, and where the dashboard lives. `config.sample.toml`: a
commented `metrics_listen` example.

### Testing

- Unit: label sanitising (reset reasons, model, firmware, `other`); device store
  merge-by-timestamp (seed older than live, live missing a field keeps last-known,
  RSSI latest-only); battery samples cap and scrape-time window (offline device ages
  out); `CacheStatus`/`SourceState` (fresh, stale, failing, empty); process sample
  parsing from fixture `/proc` text including `btime` conversion.
- Router: the main router has no `/metrics`; the metrics router serves it with the
  OpenMetrics content type; a display request moves `tablica_http_requests_total`
  (`served`) and device gauges; an unknown token counts as `refused`; the no-image 503
  counts as `unavailable`.
- Config: `metrics_listen` equal to `listen` is rejected; absent means no listener.
- `run_poller`: outcomes and durations recorded; zero series pre-created.
- Dashboard (core dev-test): parse the JSON with `serde_json`, collect every target
  `expr` and every templating variable query, substitute Grafana variables
  (`$device` → `.*`, `$__range` → `1d`, `$__rate_interval` → `5m`, …), and parse each
  with the `promql-parser` crate (dev-dependency), so invalid PromQL fails the test.
  Then check every metric name used appears in a scrape of a test server with sample
  state covering optional values (a never-seen device, a battery-less device).
  `process_*` names are checked only on Linux (`#[cfg(target_os = "linux")]`); the
  datasource of every panel/target must be `Prometheus`.
- Homelab, manual (in the plan): after deploy, the `tablica` target is up; the device
  variable lists every configured device; the status table shows a row per device
  including one with missing telemetry; resets and problem-request panels show their
  "none" message on a healthy run; the remaining panels show data.

## Calls I made (for review)

- Device label is the config name; MACs never appear.
- Resets counter is per-process with zero-initialised series; the last-reset timestamp
  gauge is seeded.
- Last-seen colouring uses lateness against the handed-out next check-in, not age.
- Metrics port 9101, unpublished, monitoring network only.
- Dashboard JSON lives in this repo under `contrib/grafana/` and is copied to homelab.

## Left to the builder

Exact `prometheus-client` version and collector API shape, `libc` vs `procfs`, the
model allowlist contents, render histogram buckets, panel layout details.
