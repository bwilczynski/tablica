# Device diagnostics screen — design

Issue: #16.

## Intent

Once the TRMNL units hang on the wall, three questions keep coming up: how long
will the battery last, is the Wi-Fi signal good enough where the unit hangs, and
why did it misbehave last night. Every `/api/display` request already carries
the answers in its headers, but tablica only writes them to the access log and
forgets them. This change keeps that telemetry (and the device's own log
entries) in a small history, and renders a per-device diagnostics screen from
it in a browser, without waking the device.

The screen must work for every dashboard. Device health is the same everywhere
and core owns it; each dashboard adds its own section (family: how old the
calendar, timetables and weather are). Homelab is a placeholder today and adds
nothing yet, but must be able to add a section later without touching core.

## What the devices send (firmware 1.8.19, `xiao_epaper_display`)

Checked against `usetrmnl/trmnl-firmware` and the live access log.

| Header | Meaning |
|---|---|
| `Battery-Voltage` | Volts, read before Wi-Fi starts. `0.00` = no battery connected. |
| `RSSI` | dBm (family unit: about −84 where it hangs now). |
| `WiFi-Band` | Optional. |
| `Wake-Time` | Milliseconds the **previous** wake took, boot to sleep (`iPrevWakeTime`, RTC memory; 0 after a power cycle). |
| `Reset-Reason` | `ESP_RST_*` name (`ESP_RST_DEEPSLEEP` is the normal wake; also `POWERON`, `SW`, `PANIC`, `INT_WDT`, `TASK_WDT`, `WDT`, `BROWNOUT`, `EXT`, `SDIO`, `UNKNOWN`, or a bare number). |
| `Update-Source` | Wake cause: `timer`, `button`, `powercycle`, others. |
| `Image-Cached` | `true`/`false`: whether the previous wake used a cached image. |
| `Refresh-Rate` | Seconds the device last slept for. |
| `FW-Version`, `FW-Commit`, `Model`, `Panel-Rev` | Identity. `FW-Commit` and `Panel-Rev` optional. |

`POST /api/log` bodies are `{"logs": [...]}`; each entry has `created_at`,
`message`, `level` (`debug`…`fatal`), `wake_reason`, `source_path`,
`source_line`, `battery_voltage`, `wifi_signal`, `firmware_version` and more.
The firmware only submits entries at error level and above.

## Goals

- Every authenticated `/api/display` request stores one **reading** per device in
  SQLite; every `/api/log` entry is stored as a **device log** row.
- A diagnostics screen per device, 800×480, at 1 and 2 bits, showing:
  1. **Power:** voltage, estimated %, and days left from the trend; "no battery"
     when the device reports 0 V.
  2. **Wi-Fi:** RSSI with a good/ok/poor label, band, and wake time (last value
     and 24 h median).
  3. **Last problem:** the most recent reading whose reset reason is not
     `ESP_RST_DEEPSLEEP`, and the most recent device log entry, each with its
     time.
  4. **Dashboard sections:** whatever the device's dashboard contributes.
  5. **Footer:** firmware version (+commit), model, tablica version, last
     check-in, next expected wake.
- Opened in a browser at `/preview/diag.png?device=<name>[&depth=1|2][&at=…]`.
- A dashboard contributes diagnostics through one new trait method; adding a
  section never touches core.

## Non-goals

- **Showing diagnostics on the device** (the button / `special_function` path).
  Deferred to a follow-up; see "Follow-up: device path" below. `/api/display`
  keeps answering `"special_function": "none"`.
- A web UI, charts or a JSON history API. (The history is a plain SQLite file;
  `sqlite3` reads it.)
- Access control on the preview. tablica is LAN-only; `/preview/*` stays open,
  diagnostics included.
- Alerting (push, email) on low battery or bad signal.
- Dashboards writing their own history to the database.
- The TRMNL X fuel-gauge headers (`Percent-Charged`, `Gauge-*`).

## Anti-goals

- A display request that fails, slows down or changes because of history.
  History is best-effort: errors are logged, never surfaced to the device, and
  the `/api/display` reply is byte-for-byte what it is today.
- History work starving dashboard rendering.
- tablica failing to start because of history.
- An unbounded database file.
- Battery numbers that look precise but are noise: no days-left estimate until
  there is enough history.

## Approaches considered

### History storage

- **JSON-lines files per device** (issue's proposal). No dependency, but needs
  capping/rewriting code, crash-tolerant parsing of a half-written last line, and
  two files per device once logs are kept too.
- **SQLite via `rusqlite` with the `bundled` feature (chosen).** One file,
  `data_dir/history.db`. Retention is one `DELETE`, writes are crash-safe, and
  "what happened last night" is a query. Cost: SQLite's C source compiles in the
  build (gcc is already in the `rust:bookworm` build image), and the README's
  "no admin UI or database" line changes to say there is no database *server*.

### Per-dashboard content

- **A. Dashboards return plain data; core draws it (chosen).** A trait method
  returns sections of rows (label, value, status, optional note). Uniform look,
  cheap per board; a board cannot draw custom graphics.
- **B. Each dashboard ships its own template block.** Full freedom, but a
  template per board and no uniform look.

### Where to open it

- **Browser only (chosen).** No firmware dependency, no device state.
- **Browser and device button.** Every firmware 1.8.19 action that shows a
  server image on a button wake (`guest_mode`, `identify`) also clears the
  device's "registered" flag, and the device learns its stored function only on
  a normal wake, so changing or rolling back the function can strand it.
  Needs a hardware test first; deferred.

## Design

### Module layout (all in `tablica-core`)

```
crates/core/src/history/mod.rs      History handle: writer task, queries, prune
crates/core/src/history/schema.sql  CREATE TABLE IF NOT EXISTS …, user_version
crates/core/src/diagnostics/mod.rs  Section, Row, Status, source_row (public, used by dashboards)
crates/core/src/diagnostics/battery.rs  voltage→% curve, days-left estimate
crates/core/src/diagnostics/view.rs     builds the template context from history + dashboard sections
crates/core/templates/diag.html         the screen (core's own embedded template)
```

Diagnostics is a core feature, not a `Dashboard` crate: it needs per-device
state and must be reachable for every device whatever dashboard it shows.

### Device identity

- Config validation rejects two devices with the same `name` (today only
  dashboards are checked); the preview looks devices up by name.
- History rows are keyed by the device's MAC in canonical form (`mac_key`), not
  by name, so renaming a device keeps its history and a replaced unit (new MAC)
  starts clean. The preview resolves name → MAC through the config. Two
  devices with the same normalized MAC fail config validation, since they
  would share one history.

### Storage

**Opening.** At startup core creates `data_dir` if missing, then opens
`data_dir/history.db` (WAL mode, `synchronous=NORMAL`) and applies the schema.

- If it cannot be opened, tablica logs an ERROR and runs **without history**:
  readings and logs are dropped, the diagnostics screen says "history
  unavailable". Startup never fails because of history.
- Only a confirmed corruption (`SQLITE_CORRUPT` / `SQLITE_NOTADB` on open or on
  `PRAGMA quick_check`) is recovered: with the connection closed, `history.db`,
  `history.db-wal` and `history.db-shm` are renamed to
  `*.corrupt-<unix ts>`, a fresh database is created, and a WARN is logged. At
  most the 3 newest corrupt sets are kept; older ones are deleted.

**Schema**, versioned with `PRAGMA user_version` (version 1):

```sql
CREATE TABLE readings (
  id            INTEGER PRIMARY KEY,
  device        TEXT    NOT NULL,   -- canonical MAC (mac_key)
  at            INTEGER NOT NULL,   -- unix seconds, server clock
  battery_v     REAL,               -- NULL if header missing/unparseable; 0.0 kept as sent
  rssi          INTEGER,
  wifi_band     TEXT,
  wake_ms       INTEGER,
  reset_reason  TEXT,
  update_source TEXT,
  image_cached  INTEGER,            -- 0/1/NULL
  refresh_rate  INTEGER,            -- header: seconds the device last slept
  next_refresh  INTEGER,            -- seconds tablica answered with
  fw_version    TEXT,
  fw_commit     TEXT,
  model         TEXT,
  panel_rev     TEXT,
  render_ms     INTEGER,            -- NULL when the render failed
  image_bytes   INTEGER
);
CREATE INDEX readings_device_at ON readings(device, at);

CREATE TABLE device_logs (
  id          INTEGER PRIMARY KEY,
  device      TEXT    NOT NULL,     -- canonical MAC
  received_at INTEGER NOT NULL,     -- unix seconds, server clock
  created_at  INTEGER,              -- device's own timestamp, if parseable
  level       TEXT,
  message     TEXT,                 -- truncated to 1 KiB
  wake_reason TEXT,
  raw         TEXT    NOT NULL      -- the entry as JSON, truncated to 4 KiB
);
CREATE INDEX device_logs_device_at ON device_logs(device, received_at);
```

String header values are capped at 64 bytes. Unparseable values are NULL.

**Writes go through one writer.** `History` owns a dedicated OS thread holding
the only write connection, fed by a bounded channel (capacity 256). Handlers
enqueue with `try_send` and never wait: when the queue is full the item is
dropped and counted, and a WARN is logged at most once a minute with the drop
count. Each `/api/log` body is one queue item, written in one transaction.
Queued bodies also share a budget of 8 MiB: a body that would exceed it is
dropped and counted the same way, and its bytes leave the budget when the
writer takes it. On
shutdown the channel closes, and the writer drains what is queued and exits.
The writer is joined with a 5 s limit. Nothing history-related runs on Tokio's
blocking pool, so rendering never waits on SQLite.

**Reads** (preview only) use a separate read-only connection on a dedicated,
detached thread (not `spawn_blocking`: the runtime waits for blocking-pool
tasks on shutdown, so a stalled read there could hold up exit) with a
`busy_timeout` of 2 s. The handler waits 3 s for its result; a read that fails
or times out renders the screen with "history unavailable" instead of failing
the preview.

**Size bounds.** Each device keeps at most the newest 2,000 log rows and the
newest 20,000 readings (about 90 days at ~220 a day), by `(received_at, id)`
and `(at, id)`. Both caps hold on every insert, in the insert's transaction: a
log batch keeps only its newest 2,000 entries, and after inserting the device
is cut back to its cap through the `(device, …)` index. Daily on the writer
thread, and once at startup:
- delete readings and logs older than 90 days;
- apply both per-device caps again;
- `PRAGMA wal_checkpoint(TRUNCATE)`.
With ~70 readings a day per device, retention binds first (6,300 readings);
a chatty device stops at 20,000 readings and 2,000 logs (≤ ~10 MB worst case
with full-size log entries) per device. Freed pages
are reused, so the file stays at its high-water mark; it is not vacuumed. The
README states this and the corrupt-file cap.

### Recording

- `display()`: parses the telemetry headers into a reading, then after the
  reply is built fills in `render_ms`, `image_bytes` and `next_refresh` (the
  `refresh_rate` it answered), and enqueues it. Nothing about the reply
  changes.
- `device_log()`: enqueues the whole body as one item (it still logs each
  message as today); the body is never cut, since a cut body is no longer
  valid JSON. A body that is not JSON is stored as one row with `raw` set to
  the lossy UTF-8 body, truncated, other fields NULL. For a body over 1 MiB
  (whatever it holds, blank included) history queues only a small marker
  instead of the body, outside the byte budget but like any item dropped and
  counted when the queue is full; the writer stores it, unparsed, as one row
  with `raw` set to `body too large (<N> bytes)`. The handler still parses the
  original body to log its messages, as for any other body.
- Refused requests (unknown token) are not stored.

### Diagnostics contract for dashboards

```rust
pub enum Status { Ok, Warn, Bad, Unknown }

pub struct Row {
    pub label: String,
    pub value: String,
    pub status: Status,
    pub note: Option<String>,   // e.g. the last error, shortened by the template
}

pub struct Section {
    pub title: String,
    pub rows: Vec<Row>,
}

// added to `Dashboard`
fn diagnostics(&self, now: DateTime<Utc>) -> Vec<Section> { Vec::new() }
```

Core also exports `diagnostics::source_row(label, &SourceCache<T>, now,
max_age) -> Row`, using the same rules as `source_health`: age of `fetched_at` as
the value, `Bad` when there is no data, `Warn` when older than `max_age`, the last
error as the note. Family implements `diagnostics()` with one "Sources" section:
calendar, weather, and one timetable row per kid (labelled with the kid's name).
Homelab keeps the default (no sections); the screen shows only the core panels.

Dashboard sections always describe the **current** state: dashboards keep only
their latest data, so they are called with the real now, never with `?at=`.

### Derived values

- **Battery %:** piecewise-linear LiPo curve on voltage (single cell, at rest):
  4.20→100, 4.10→90, 4.00→80, 3.90→65, 3.80→50, 3.75→40, 3.70→30, 3.65→20,
  3.60→12, 3.50→5, 3.30→0, clamped. Voltage is smoothed as the median of the
  last 4 non-zero readings, so one noisy read does not swing the number. 0.0 V
  readings are left out of the smoothing and the trend; when the latest reading
  is 0.0 V the panel shows "no battery", with no % and no estimate.
- **Days left:** least-squares slope of the estimated % over the last 7 days of
  readings (fewer if less exists), extrapolated to 0 %. Shown only when the
  readings span at least 48 hours and the slope is negative. Otherwise
  "collecting data (N h)". A jump up of ≥ 10 % (recharge) in the rolling median
  of three readings restarts the window, so one odd reading does not. Estimates
  above 90 days read ">90 days".
- **RSSI label:** ≥ −67 dBm good, −68…−75 ok, < −75 poor (`Bad`).
- **Wake time:** last value and median over 24 h; `Warn` when the median is above
  10 s. A `Wake-Time` of 0 right after a power cycle is ignored.
- **Last problem:** the latest reading with `reset_reason` not
  `ESP_RST_DEEPSLEEP`. `BROWNOUT`, `PANIC` and the `*WDT` reasons are `Bad`;
  `POWERON`, `SW`, `EXT` are `Ok` (expected after plugging in or flashing);
  others `Warn`. The latest device log row with its level and time. Each says
  "none in 90 days" when absent.
- **Next wake:** `at + next_refresh` of the latest reading.
- **Last check-in:** `at` of the latest reading, shown relative ("12 min ago")
  and absolute in the configured timezone.

### Rendering

`diag.html` is a core template, embedded in `tablica-core` like the dashboards'
own bundles and rendered by the same `Renderer` (MiniJinja → Takumi → quantize
→ PNG), with its own environment named `diag`. In `TemplateSource::Dir` mode a
`diag.html` in that directory wins; when the directory has none, the embedded
copy is used, so existing `--templates` directories keep working.

Layout: power and Wi-Fi side by side on top, last problem below, then the
dashboard sections, footer along the bottom. Text is English. It must stay
under `MAX_PNG_BYTES` and readable at 1 bit; status shows as a word or symbol,
never by grey level alone. The sections get 10 lines: each kept section's
heading takes one and each row one; sections with no rows (or no room for a
row) are left out, and the rows that do not fit are counted in "+N more". A
device with no readings yet shows "no check-in yet" in the device panels.

Diagnostics PNGs are returned directly by the preview and never go through
`ImageStore`, so they can never be served to a device as its latest image.

### Browser access

`GET /preview/diag.png?device=<name>[&depth=1|2][&at=<rfc3339>]`:
- missing `device` → 400; unknown device → 404;
- device panels come from history as of `at` (default now); dashboard sections
  are always current and the section header says "now" when `at` is given;
- a render failure → 500, as for other previews (browser only; the device API
  is untouched).

`diag` has 4 letters and dashboard names have 5–7, so the path cannot collide
with a dashboard.

### Footer versions

tablica version from `env!("CARGO_PKG_VERSION")` in core (the workspace shares
one version).

### Docs

- README: replace "no admin UI or database" with wording that says there is no
  database server; describe `history.db` in `data_dir`, its size bounds and
  corrupt-file handling, and the preview URL.
- README "add a dashboard" section: mention `diagnostics()` and `source_row`.
- `/healthz` is unchanged.

## Testing

- `History`: tempfile DB; record + query round trips; a log body is atomic;
  retention and the per-device log cap delete only what they should; a corrupt
  file (with `-wal`/`-shm`) is moved aside and the old-set cap holds; an
  unopenable path leaves tablica running without history; a full queue drops
  without blocking; shutdown drains the queue.
- Device identity: duplicate device names, and two devices with the same MAC
  once normalized, fail config validation; a renamed device keeps its history
  (keyed by MAC).
- Battery: curve points, clamping, smoothing, days-left with synthetic series
  (steady drain, too little data, recharge jump, 0 V readings).
- Device API (existing router tests): display records a reading with parsed
  headers, and its JSON reply is unchanged (`special_function: "none"`); log
  entries are stored; a history failure does not change either response.
- Preview: 400/404 cases; a valid PNG at both depths under `MAX_PNG_BYTES`;
  `at` filters device history but not dashboard sections; works with a
  `--templates` dir that has no `diag.html`.
- Family: `diagnostics()` returns a Sources section with one row per kid.
- `cargo test write_previews` also writes `target/preview/diag-*.png` for a
  battery device with history and for a device with no history yet.

## Follow-up: device path (not in this change)

Notes from firmware v1.8.19 for whoever picks it up:
- The stored special function comes from the `special_function` field of a
  normal reply and is saved only on a successful normal wake. A double-click
  (or a 1–5 s hold) then sends `special_function: true`, and the reply's
  `action` must match the stored function.
- `guest_mode` shows `image_url` and applies `refresh_rate`; `identify` shows it
  but keeps the previous sleep interval (up to the ~8 h night interval). Both
  clear the "registered" flag for a non-`empty_state` image.
- Changing or rolling back the function needs a normal wake before the button
  works with the new one.
- Diagnostics images served to the device would need their own `ImageStore`
  kind, so `latest()` never falls back to a diagnostics image.
- Test on the homelab unit what clearing the "registered" flag does before
  choosing.

## Left to the builder

- Exact SQL for queries, and whether to keep a cached "latest reading" in memory.
- Template markup and typography, within the layout above.
- Wording of labels and how relative times are formatted.
