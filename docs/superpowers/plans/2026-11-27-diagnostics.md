# Device Diagnostics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep each device's telemetry and log entries in a SQLite history and render a per-device diagnostics PNG at `/preview/diag.png?device=<name>`, with sections contributed by each dashboard.

**Architecture:** A new `history` module in `tablica-core` owns `data_dir/history.db` through one writer thread fed by a bounded channel; the device API enqueues readings and raw log bodies without waiting, and the writer parses, stores and prunes. A new `diagnostics` module defines the dashboard contract (`Section`/`Row`/`Status`), derives battery/Wi-Fi/problem values from a history snapshot, and builds the context for a core-owned `diag.html` rendered by the existing `Renderer`.

**Tech Stack:** Rust 2024 workspace, axum 0.8, tokio, rusqlite (`bundled`), minijinja, Takumi.

**Spec:** `docs/superpowers/specs/2026-11-27-diagnostics-design.md`

## Global Constraints

- The `/api/display` and `/api/log` replies stay byte-for-byte as today (`"special_function": "none"`); history never changes or delays a device reply.
- Startup never fails because of history; an unusable database means "history unavailable", not an error exit.
- History writes and maintenance run only on the writer thread. Preview reads run on the blocking pool, at most one at a time, with a deadline.
- History rows are keyed by the device's normalized MAC (`Device::mac`, already `AA:BB:CC:DD:EE:FF` after config load), never by name.
- Rows are ordered by `(at, id)` / `(received_at, id)` everywhere: queries, "latest", pruning.
- Constants (in `history/mod.rs`): `QUEUE_CAPACITY = 256`, `RETENTION_DAYS = 90`, `MAX_LOGS_PER_DEVICE = 2000`, `HEADER_MAX = 64`, `MESSAGE_MAX = 1024`, `RAW_MAX = 4096`, `LOG_BODY_MAX = 65_536`, `CORRUPT_KEEP = 3`, `SHUTDOWN_WAIT = 5 s`, `PRUNE_EVERY = 24 h`, `READ_DEADLINE = 2 s`. Truncation is at a UTF-8 char boundary.
- Header validation (else `None`): `Battery-Voltage` finite and in `0.0..=10.0`; `RSSI` in `-150..=0`; `Wake-Time` in `0..=86_400_000`; `Refresh-Rate` in `0..=604_800`.
- Screen text is English. PNG at 800×480, both depths, under `render::MAX_PNG_BYTES`; status is a word, never grey alone.
- `rusqlite` added to `[workspace.dependencies]` with `features = ["bundled"]`, pinned to an exact released version at least two weeks old; `Cargo.lock` committed with it.
- Commits: conventional (`feat:`, `fix:`, `docs:`, `test:`), no agent attribution trailers.
- Verify each task with `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` before committing.

## Review Focus

1. Malformed or hostile telemetry (`RSSI: abc`, `Battery-Voltage: NaN`, `Wake-Time: -5`, a 10 KB `FW-Version`) → reply bytes unchanged, stored as NULL / truncated. Test in Task 3.
2. A burst beyond 256 queued items while the writer is stalled → handlers never wait, extras dropped and counted. Test in Task 2.
3. `data_dir` unusable for the database → tablica serves both device endpoints and the preview shows "history unavailable". Tests in Tasks 2, 3 and 7.
4. A device last seen 8 days ago, and a preview `at` before the first reading or in the future → latest reading still shown / "no check-in yet", never an error. Tests in Tasks 2 and 6.
5. A dense screen (long source errors, a long device log, many sections) → footer stays on screen and the PNG stays under the size limit at both depths. Test in Task 6.

---

### Task 1: Reject duplicate device names

**Files:**
- Modify: `crates/core/src/config.rs` (`load`, after building `devices`; `ConfigError`)
- Test: `crates/core/src/config.rs` tests

**Interfaces:**
- Produces: `ConfigError::DuplicateDevice { device: String }`, message ``"device `{device}` is configured more than once"``.

- [ ] **Step 1: Write the failing test** `duplicate_device_names_are_rejected`: two `[[device]]` entries named `kitchen` (different MACs) → `Err(ConfigError::DuplicateDevice { device })`, `device == "kitchen"`. Reuse the module's config-writing helpers.
- [ ] **Step 2: Run** `cargo test -p tablica-core duplicate_device_names` — expect FAIL.
- [ ] **Step 3: Implement** the variant and a `HashSet<&str>` check before the dashboard lookup.
- [ ] **Step 4: Run** `cargo test -p tablica-core` — expect PASS.
- [ ] **Step 5: Commit** `fix: reject two devices with the same name`.

### Task 2: History store

**Files:**
- Modify: `Cargo.toml`, `Cargo.lock`, `crates/core/Cargo.toml`, `crates/core/src/lib.rs` (`pub mod history;`)
- Create: `crates/core/src/history/mod.rs` (types, `History` handle, queue, writer loop), `crates/core/src/history/store.rs` (SQL: open/recover, schema, inserts, prune, snapshot), `crates/core/src/history/logs.rs` (log body → rows), `crates/core/src/history/schema.sql` (spec schema, `CREATE … IF NOT EXISTS`, plus `CHECK (device <> '')` on both tables)

**Interfaces:**
- Consumes: `device_api::Clock` (`Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>`).
- Produces:
```rust
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct Reading {
    pub device: String,              // canonical MAC
    pub at: DateTime<Utc>,
    pub battery_v: Option<f64>, pub rssi: Option<i64>, pub wifi_band: Option<String>,
    pub wake_ms: Option<i64>, pub reset_reason: Option<String>, pub update_source: Option<String>,
    pub image_cached: Option<bool>, pub refresh_rate: Option<i64>, pub next_refresh: Option<i64>,
    pub fw_version: Option<String>, pub fw_commit: Option<String>, pub model: Option<String>,
    pub panel_rev: Option<String>, pub render_ms: Option<i64>, pub image_bytes: Option<i64>,
}
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DeviceLog {
    pub device: String, pub received_at: DateTime<Utc>, pub created_at: Option<DateTime<Utc>>,
    pub level: Option<String>, pub message: Option<String>, pub wake_reason: Option<String>,
    pub raw: String,
}
/// What the diagnostics screen reads for one device as of `at`, from one read transaction.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DeviceHistory {
    pub latest: Option<Reading>,         // newest with at <= `at`, any age
    pub readings: Vec<Reading>,          // at in (at-7d, at], oldest first (trend data)
    pub last_problem: Option<Reading>,   // newest with reset_reason not ESP_RST_DEEPSLEEP, at <= `at`
    pub last_log: Option<DeviceLog>,     // newest by (received_at, id) with received_at <= `at`
}
pub struct History { /* see below */ }
impl History {
    pub fn open(data_dir: &Path, clock: Clock) -> History;   // never fails
    pub fn disabled() -> History;
    pub fn is_available(&self) -> bool;
    pub fn record_reading(&self, r: Reading);                 // truncates strings, try_send
    pub fn record_log_body(&self, device: &str, received_at: DateTime<Utc>, body: &[u8]); // copies at most LOG_BODY_MAX bytes, try_send
    pub fn flush(&self);                                      // blocks until everything queued before it is handled
    pub fn shutdown(&self);                                   // = shutdown_within(SHUTDOWN_WAIT)
    pub fn shutdown_within(&self, limit: Duration) -> bool;   // true if the writer finished
    pub fn snapshot(&self, device: &str, at: DateTime<Utc>) -> anyhow::Result<DeviceHistory>; // blocking
    pub fn dropped(&self) -> u64;
}
// logs.rs
pub fn parse_log_body(device: &str, received_at: DateTime<Utc>, body: &[u8]) -> Vec<DeviceLog>;
```

**Handle and writer.**
- `History` holds `sender: RwLock<Option<SyncSender<Op>>>` (producers take the read lock and `try_send`; `shutdown` takes the write lock and `take()`s it). It also holds `done: Mutex<Option<Receiver<()>>>`, signalled by the writer as its last act, and `join: Mutex<Option<JoinHandle<()>>>`.
- `shutdown_within(limit)` drops the sender, waits `done.recv_timeout(limit)`, and joins only after `done` arrived; on timeout it logs a WARN and leaves the thread detached. Calling it twice is a no-op. `Drop for History` drops the sender without waiting, so error paths in `server::run` before serving close the queue.
- `Op`: `Reading(Reading)`, `LogBody { device, received_at, body: Vec<u8> }`, `Prune(DateTime<Utc>)` (tests), `Flush(SyncSender<()>)`, and `#[cfg(test)] Stall(Receiver<()>)`.
- The writer loop uses `recv_timeout` and runs prune itself: once at start, then whenever `PRUNE_EVERY` has passed by `clock()`. Maintenance never goes through the lossy queue.
- A full queue increments `dropped` and logs a WARN at most once per minute with the count.
- `LogBody` is parsed on the writer by `parse_log_body`: one row per `logs[]` entry, written in one transaction. `created_at` accepts unix seconds (number) or an RFC 3339 string; `raw` is the entry re-serialized; a non-JSON body becomes one row with `raw` = lossy UTF-8. All strings are truncated per the constants.

**Store.**
- Open: `create_dir_all(data_dir)`, open `history.db`, `journal_mode=WAL`, `synchronous=NORMAL`, `PRAGMA quick_check`. Read `user_version`: 0 → apply schema and set 1; 1 → apply schema (idempotent); > 1 → ERROR, disabled.
- Recovery only on `SQLITE_CORRUPT`/`SQLITE_NOTADB` (open, pragma, or quick_check not `ok`): close, rename the existing `history.db`, `-wal`, `-shm` to `history.db.corrupt-<unix secs>-<seq>{,-wal,-shm}`, where `<seq>` makes the set name unique. Keep the newest `CORRUPT_KEEP` sets (ordered by secs then seq), delete older ones, reopen once. Any other error → disabled.
- Prune: one transaction that deletes readings and logs older than `now - RETENTION_DAYS` and, per device, all but the newest `MAX_LOGS_PER_DEVICE` logs by `(received_at, id)`; **commit**, then `PRAGMA wal_checkpoint(TRUNCATE)`. A busy or failed checkpoint is logged at DEBUG and never undoes the prune.
- Snapshot: a fresh read-only connection, `busy_timeout(READ_DEADLINE)`, a progress handler that aborts once `READ_DEADLINE` has passed, all four queries in one `BEGIN DEFERRED` read transaction. The connection is dropped before returning.

- [ ] **Step 1: Write failing tests** (tempdir per test; `flush()` before asserting):
  - `reading_round_trips`: a fully populated `Reading` → `snapshot(mac, at)` returns it as `latest` and in `readings`.
  - `strings_are_truncated`: 10_000-char `fw_version` → 64; a log `message` of 5_000 chars → 1024; `raw` → 4096; a multi-byte string is cut on a char boundary.
  - `snapshot_window`: readings at at−8d, at−1d, at+1h → `readings` holds at−1d only, `latest` is at−1d; a brownout at at−30d is `last_problem` and a later `ESP_RST_DEEPSLEEP` does not replace it.
  - `offline_for_eight_days`: only a reading at at−8d → `readings` empty, `latest` is that reading.
  - `log_batch`: a body with 3 entries → 3 rows sharing `received_at`; `last_log` is the third entry (highest id).
  - `log_batch_rolls_back` (store-level): `insert_logs` with 3 rows where the second has `device: ""` (violates the CHECK) → error and 0 rows stored.
  - `prune_deletes_only_old_rows`: rows at now−91d and now−89d → only the 89-day rows survive; 2_005 logs for one device → newest 2_000 remain (by `(received_at, id)`), the other device untouched; the WAL file is truncated afterwards.
  - `writer_prunes_on_start`: a database with a now−91d row, `open` with a clock at now → after `flush()`, the row is gone.
  - `corrupt_file_is_moved_aside`: garbage in `history.db`, `-wal`, `-shm` → `open` is available and one complete set (3 files) exists; four corruptions in a row (same second) → exactly the newest 3 complete sets remain.
  - `newer_schema_is_refused`: `user_version = 2` → `!is_available()`, file untouched.
  - `unusable_path_disables_history`: `data_dir` is a regular file → `!is_available()`; recording does not panic; `snapshot` is `Err`.
  - `full_queue_never_blocks`: stall the writer with `Op::Stall`, enqueue 300 readings, the loop returns in < 100 ms and `dropped() >= 44`; release; `flush()`.
  - `shutdown_drains_queue`: 10 readings, `shutdown()`, reopen → 10 readings.
  - `shutdown_gives_up_on_a_stalled_writer`: stall the writer, `shutdown_within(100 ms)` returns `false` in < 1 s.
- [ ] **Step 2: Run** `cargo test -p tablica-core history` — expect FAIL.
- [ ] **Step 3: Add the dependency** (`rusqlite = { version = "=<exact>", features = ["bundled"] }`, `rusqlite.workspace = true` in core).
- [ ] **Step 4: Implement** to the interfaces above.
- [ ] **Step 5: Run** `cargo test -p tablica-core history` — expect PASS; clippy, fmt clean.
- [ ] **Step 6: Commit** (including `Cargo.lock`) `feat: keep device telemetry and logs in a SQLite history`.

### Task 3: Record from the device API; wire into the server

**Files:**
- Create: `crates/core/src/device_api/telemetry.rs`
- Modify: `crates/core/src/device_api/mod.rs` (`AppState`, `display`, `device_log`, test fixture), `crates/core/src/server.rs`, `crates/tablica/src/render_tests.rs`, `crates/tablica/tests/end_to_end.rs`

**Interfaces:**
- Consumes: Task 2.
- Produces:
  - `AppState { …, pub history: Arc<History> }`; test builders use `Arc::new(History::disabled())` unless asserting on history.
  - `telemetry::reading(mac: &str, at: DateTime<Utc>, headers: &HeaderMap) -> Reading` — validation per Global Constraints; `Image-Cached` `"true"`/`"false"`; strings as sent (truncation happens in `record_reading`).
  - `prepare_state` opens `History::open(&cfg.data_dir, clock.clone())`. `run` calls `history.shutdown()` inside `spawn_blocking` after the pollers stop. Early returns before serving rely on `Drop`.

`display` builds the reading before rendering, then sets `render_ms` (elapsed of `render_blocking`, `None` on failure), `image_bytes` (PNG length; `None` when an older image was reused), `next_refresh` (the `refresh_rate` answered), and calls `record_reading` right before returning. The 503 path records too, with those three `None`. `device_log` keeps its tracing output unchanged and calls `record_log_body` with the raw body. Refused requests record nothing.

- [ ] **Step 1: Write failing tests** in `device_api/mod.rs` (fixture with `History::open(tempdir, clock)`):
  - `display_records_a_reading`: `Battery-Voltage: 4.01`, `RSSI: -84`, `Wake-Time: 4123`, `Reset-Reason: ESP_RST_DEEPSLEEP`, `Update-Source: timer`, `Image-Cached: true`, `Refresh-Rate: 900`, `FW-Version: 1.8.19`, `Model: xiao_epaper_display` → after `flush()`, `latest` for `AA:BB:CC:DD:EE:01` carries those values, `next_refresh == Some(900)`, `render_ms` and `image_bytes` set.
  - `hostile_headers_are_null_and_reply_unchanged`: `RSSI: abc`, `Battery-Voltage: NaN`, then `inf`, `Wake-Time: -5`, `Refresh-Rate: 99999999`, 10 KB `FW-Version` → the full response body bytes equal those of the same request without telemetry headers (pinned clock); stored numeric fields `None`, `fw_version` 64 bytes.
  - `unknown_token_records_nothing`.
  - `device_log_stores_entries`: `{"logs":[{"created_at":1795810000,"message":"HTTP GET failed","level":"error","wake_reason":"timer"}]}` → 204, `last_log.message == Some("HTTP GET failed")`, `created_at` set; body `not json` → 204, `last_log.raw == "not json"`.
  - `history_keeps_following_a_renamed_device`: record for `kitchen`, rebuild state with the device renamed `galley` (same MAC, same data_dir) → snapshot by MAC still has the reading.
  - `display_and_log_with_history_disabled`: `History::disabled()` → existing reply assertions pass, `/api/log` → 204.
  - existing `display_returns_numeric_refresh_rate_and_stable_filename` still asserts `special_function == "none"`.
  - end_to_end `serves_with_unusable_history`: `data_dir` points at a regular file for history (`prepare_state` with that config) → `prepare_state` is `Ok`, `/api/display` 200, `/api/log` 204.
- [ ] **Step 2: Run** `cargo test --workspace` — expect FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace` — expect PASS.
- [ ] **Step 5: Commit** `feat: record device telemetry and log entries in the history`.

### Task 4: Diagnostics contract and the family section

**Files:**
- Create: `crates/core/src/diagnostics/mod.rs`; Modify: `crates/core/src/lib.rs` (`pub mod diagnostics;`), `crates/core/src/dashboard.rs`, `crates/family/src/dashboard.rs`, `crates/homelab/src/lib.rs` (test only)

**Interfaces:**
- Produces:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status { Ok, Warn, Bad, Unknown }
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Row { pub label: String, pub value: String, pub status: Status, pub note: Option<String> }
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Section { pub title: String, pub rows: Vec<Row> }
pub fn format_age(age: chrono::Duration) -> String;   // < 60 s or negative "just now"; < 60 min "N min"; < 48 h "N h"; else "N d"
pub fn source_row<T: Serialize + DeserializeOwned + Clone>(label: &str, cache: &SourceCache<T>, now: DateTime<Utc>, max_age: Duration) -> Row;
// Dashboard trait:
fn diagnostics(&self, now: DateTime<Utc>) -> Vec<Section> { Vec::new() }
```
`source_row`: no data → `("no data", Bad)`; stale per `cache::is_stale` → `(format_age, Warn)`; else `(format_age, Ok)`; `note` = last error. Family: one section `"Sources"` with rows `calendar`, `weather`, then `timetable <kid label>` per kid, using the family `*_MAX_AGE` constants. Homelab keeps the default.

- [ ] **Step 1: Write failing tests**: `format_age_buckets` (30 s "just now", 12 min "12 min", 3 h "3 h", 50 h "2 d", −5 min "just now"); `source_row_statuses` (empty → no data/Bad; 1 h old, max 2 h → "1 h"/Ok; 3 h → Warn; `record_error("login failed")` → note); family `diagnostics_lists_every_source` (2 kids → labels `["calendar", "weather", "timetable <kid0>", "timetable <kid1>"]`, all Bad on empty caches); homelab `diagnostics_is_empty`.
- [ ] **Step 2: Run** `cargo test --workspace diagnostics` — expect FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace` — expect PASS.
- [ ] **Step 5: Commit** `feat: let dashboards contribute diagnostics sections`.

### Task 5: Derived values

**Files:**
- Create: `crates/core/src/diagnostics/battery.rs`, `crates/core/src/diagnostics/derive.rs`; Modify: `crates/core/src/diagnostics/mod.rs` (`pub mod battery; pub mod derive;`)

**Interfaces:**
- Consumes: Task 2 `Reading`, Task 4 `Status`.
- Produces:
```rust
// battery.rs
pub fn percent(volts: f64) -> f64;   // curve below, linear between points, clamped 0..=100
#[derive(Debug, Clone, PartialEq)]
pub enum Estimate { Collecting { hours: i64 }, DaysLeft(f64) }
#[derive(Debug, Clone, PartialEq)]
pub enum Power { NoData, NoBattery, Battery { volts: f64, percent: f64, estimate: Estimate } }
pub fn power(latest: Option<&Reading>, readings: &[Reading]) -> Power;
// derive.rs
pub fn rssi_status(dbm: i64) -> (&'static str, Status);   // >= -67 ("good", Ok); -68..=-75 ("ok", Warn); < -75 ("poor", Bad)
pub struct Wake { pub last_ms: Option<i64>, pub median_24h_ms: Option<i64>, pub status: Status }
pub fn wake(latest: Option<&Reading>, readings: &[Reading], at: DateTime<Utc>) -> Wake;
pub fn reset_status(reason: &str) -> Status;   // ESP_RST_BROWNOUT, ESP_RST_PANIC, *_WDT → Bad; ESP_RST_POWERON, ESP_RST_SW, ESP_RST_EXT → Ok; else Warn
```
Curve (V→%): 4.20→100, 4.10→90, 4.00→80, 3.90→65, 3.80→50, 3.75→40, 3.70→30, 3.65→20, 3.60→12, 3.50→5, 3.30→0.

`power`: `latest` reports `battery_v == Some(0.0)` → `NoBattery`. Otherwise use non-zero voltages from `readings` plus `latest`'s (deduplicated by id/at): none → `NoData`; `volts` = median of the last 4. Estimate on those readings mapped through `percent`: the window starts after the last rise of ≥ 10 points between consecutive readings; span < 48 h → `Collecting { hours }`; least-squares slope (%/day) ≥ 0 → `Collecting { hours }` too (the spec's "collecting data" text); else `DaysLeft(current_percent / -slope)`.

`wake`: `last_ms` = `latest.wake_ms` when non-zero; `median_24h_ms` over non-zero values with `at - 24h < r.at <= at` (even count → mean of the two middles, computed in `i128` or as `a + (b - a) / 2`); status Warn when the median > 10_000, Unknown when there is no median, else Ok.

- [ ] **Step 1: Write failing tests**: `curve_points_and_clamping` (each point exact; 3.95 → 72.5; 4.5 → 100; 3.0 → 0); `median_smooths_a_spike` (3.90, 3.91, 3.50, 3.90 → 3.90); `steady_drain_gives_days_left` (hourly over 4 days, 4.00 → 3.80 V linearly → `DaysLeft` ≈ 6.7 ± 0.5); `too_little_history_is_collecting` (30 h → `Collecting { hours: 30 }`); `flat_or_rising_is_collecting` (4 days at 3.90 V → `Collecting`); `recharge_restarts_window` (drain then 4.20 V 10 h ago → `Collecting { hours: 10 }`); `zero_volts` (latest 0.0 → `NoBattery`; zeros inside a drain are ignored; latest with `battery_v: None` after an earlier 0.0 and earlier 3.9 V readings → `Battery`; nothing → `NoData`); `rssi_labels` (−60, −67 good; −68, −75 ok; −76, −84 poor); `wake_window_and_median` (non-zero wakes [4000, 5000, 11000, 12000] within 24 h plus a 0 → median 8000 Ok; [11000, 12000, 13000] → 12000 Warn; a reading exactly 24 h before `at` is excluded; latest wake known but no readings in 24 h → `last_ms` Some, median None, Unknown; two `i64::MAX / 2 + 1` values do not overflow); `reset_reasons` (BROWNOUT/PANIC/TASK_WDT Bad, POWERON Ok, UNKNOWN Warn, `"7"` Warn).
- [ ] **Step 2: Run** `cargo test -p tablica-core diagnostics::` — expect FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — expect PASS.
- [ ] **Step 5: Commit** `feat: derive battery, Wi-Fi and reset status from history`.

### Task 6: Diagnostics view, `diag.html`, renderer support

**Files:**
- Create: `crates/core/src/diagnostics/view.rs`, `crates/core/templates/diag.html`; Modify: `crates/core/src/diagnostics/mod.rs` (`pub mod view;`), `crates/core/src/render/mod.rs`, `crates/tablica/src/render_tests.rs`

**Interfaces:**
- Consumes: Tasks 2, 4, 5.
- Produces:
```rust
// view.rs
pub struct DiagInput<'a> {
    pub device: &'a Device,
    pub history: anyhow::Result<DeviceHistory>,   // Err → unavailable
    pub sections: Vec<Section>,
    pub at: DateTime<Utc>,
    pub at_given: bool,
    pub tz: chrono_tz::Tz,
}
#[derive(Debug, serde::Serialize)] pub struct DiagView { /* fields below */ }
pub fn diag_view(input: DiagInput<'_>) -> DiagView;   // pure; tests assert on it
pub fn build(input: DiagInput<'_>) -> View;           // template "diag.html", context = DiagView
// render/mod.rs
pub const DIAG_TEMPLATE: &str = "diag.html";
impl Renderer { pub fn render_diag(&self, view: &View, depth: BitDepth) -> anyhow::Result<Vec<u8>>; }
```
`DiagView` (pre-formatted strings + lowercase status words): `device`; `power` {kind `"battery"|"no_battery"|"no_data"`, volts `"3.92 V"`, percent `"78 %"`, estimate `"~41 days"` | `"collecting data (30 h)"`, status}; `wifi` {rssi `"-84 dBm"`, label, band, status} from `latest`, or absent; `wake` {last `"4.1 s"`, median `"8.0 s"`, status}; `problem` {reason, when, status} or text `"none in 90 days"`; `last_log` {level, message, when} or `"none in 90 days"`; `sections` (titles suffixed `" (now)"` when `at_given`); footer {fw `"1.8.19 (abc1234)"`, model, tablica `env!("CARGO_PKG_VERSION")`, checked_in `"12 min ago · 07:01"`, next_wake `"07:16"` or `"unknown"`}; `unavailable: bool`; `no_checkin: bool` (no `latest`). Times in `tz` as `HH:MM`, prefixed `dd.mm ` when not on `at`'s local day.

**Layout budget** (enforced in `diag_view`, not the template): at most 10 section rows in total across all sections, in order; the excess becomes a final row `"+N more"`. Notes are cut to 60 chars and the log message to 120 chars, with `…`. These limits keep the footer inside 480 px.

Renderer: an embedded core environment built once from `include_str!("../../templates/diag.html")`. `Embedded` mode uses it; `Dir` mode tries the reloader env and falls back to the embedded env only on `ErrorKind::TemplateNotFound`. Factor the HTML→PNG tail of `render` into a private helper shared by both. `render_diag` returns an error when the PNG exceeds `MAX_PNG_BYTES`, instead of only logging it.

Template: 800×480, Inter, black on white, spec layout (power | Wi-Fi top, problem + last log, sections, footer). Status tags `OK` / `WARN` / `BAD` / `?`; `BAD` inverted (white on black).

- [ ] **Step 1: Write failing tests**:
  - view `battery_device_context`: steady drain, brownout 2 days ago, one error log → `power.kind == "battery"`, `problem.status == "bad"`, `last_log.message` set, `next_wake` = `latest.at + next_refresh` formatted.
  - view `no_checkin_yet`: default `DeviceHistory` → `no_checkin`, power `no_data`, `next_wake == "unknown"`.
  - view `history_unavailable`: `Err` → `unavailable`, sections kept.
  - view `at_marks_sections_as_now`: `at_given: true` → section title `"Sources (now)"`.
  - view `layout_budget`: two sections of 8 rows → 10 rows shown + `"+6 more"`; 500-char note → 60 chars ending `…`; 1_000-char log message → 120.
  - render `diag_renders_both_depths_under_limit`: the battery view and a **dense** view (budget-filling rows, max-length notes and message) → 800×480 at 1 and 2 bits, each `<= MAX_PNG_BYTES`.
  - tablica `diag_falls_back_to_embedded_in_dir_mode`: `flat_templates()` (no `diag.html`) → `render_diag` equals the embedded render; after writing a custom `diag.html` there → differs.
- [ ] **Step 2: Run** `cargo test --workspace diag` — expect FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace` — expect PASS. Write the dense render to `target/preview/` once and look at it for overflow.
- [ ] **Step 5: Commit** `feat: render the device diagnostics screen`.

### Task 7: `/preview/diag.png` route

**Files:**
- Modify: `crates/core/src/device_api/mod.rs` (`PreviewQuery`, `preview`, new `diag_preview`), `crates/core/src/device_api/stub.rs` (record the `now` passed to `diagnostics`), `crates/tablica/src/render_tests.rs`

**Interfaces:**
- Consumes: Task 2 `History::snapshot`, Task 4 `Dashboard::diagnostics`, Task 6 `diag_view`/`build`/`render_diag`.
- Produces: `GET /preview/diag.png?device=<name>[&depth=1|2][&at=<rfc3339>]`, and `pub(crate) async fn diag_input(state: &AppState, device: &Device, at: Option<DateTime<Utc>>) -> DiagInput` (testable without rendering).

`preview` checks `file == "diag.png"` first. No `device` → 400 `"device is required"`; unknown name → 404; `depth`/`at` → same 400s as today. Snapshot: acquire a permit of a `tokio::sync::Semaphore` with 1 permit held in `AppState` (`pub diag_reads: Arc<Semaphore>`), then `tokio::time::timeout(READ_DEADLINE + 1 s, spawn_blocking(snapshot))` with the permit moved into the blocking closure; timeout or error → `Err` (unavailable screen). Sections: `dashboards[device.dashboard].diagnostics(clock())` — real now, never `at`. Render in its own `spawn_blocking` after the snapshot connection is gone. Render error → 500 with WARN. Never touches `ImageStore`.

- [ ] **Step 1: Write failing tests**:
  - `diag_preview_requires_known_device`: no `device` → 400; `device=nobody` → 404; `depth=3` → 400.
  - `diag_preview_renders_png`: after one recorded display for `kitchen` → 200 `image/png`, 800×480 2-bit; `&depth=1` → 1-bit.
  - `diag_input_filters_history_by_at`: readings at 06:00 and 07:00 → `at=06:30` gives `latest.at == 06:00`; `at` a year earlier → `latest` None; a year later → 07:00.
  - `diag_input_passes_real_now_to_dashboards`: the stub records the `now` it got → equals the pinned clock, not `at`.
  - `diag_preview_with_history_disabled`: 200.
  - `diag_preview_does_not_touch_image_store`: `images.latest("kitchen")` unchanged.
  - tablica `diag_preview_for_family_and_homelab`: `kitchen` (family) and `office` (homelab) → 200 PNG.
- [ ] **Step 2: Run** `cargo test --workspace diag_` — expect FAIL.
- [ ] **Step 3: Implement** (add `diag_reads` to every `AppState` builder).
- [ ] **Step 4: Run** `cargo test --workspace` — expect PASS.
- [ ] **Step 5: Commit** `feat: serve the diagnostics screen at /preview/diag.png`.

### Task 8: Preview images and docs

**Files:**
- Modify: `crates/tablica/src/render_tests.rs`, `README.md`

- [ ] **Step 1: Add** test `write_previews_diagnostics` (selected by `cargo test write_previews`) writing to the workspace `target/preview/` (path built as in `crates/family/src/render_tests.rs`): `diag-battery-2bit.png`, `diag-battery-1bit.png` (family device, 4-day drain, a brownout, an error log), `diag-dense-2bit.png`, `diag-empty-2bit.png` (homelab, no history). Run `cargo test write_previews`, look at the images, fix `diag.html` if anything overflows or is illegible at 1 bit.
- [ ] **Step 2: Update README**: "no admin UI or database" → "no admin UI and no database server"; a "Diagnostics" section (what is recorded; `data_dir/history.db`, 90-day retention, ≤ 2,000 log rows per device, the file stays at its high-water mark, up to 3 `*.corrupt-*` sets; the preview URL with `device`/`depth`/`at`; dashboard sections always current). In "add a dashboard", mention `Dashboard::diagnostics` and `diagnostics::source_row`.
- [ ] **Step 3: Run** `cargo test --workspace`, clippy, fmt — expect PASS.
- [ ] **Step 4: Commit** `docs: describe device diagnostics and the history file`.
