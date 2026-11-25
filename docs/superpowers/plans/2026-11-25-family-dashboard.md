# tablica: family dashboard implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `tablica`, a single Rust binary that serves TRMNL OG devices. It serves
a family dashboard (iCloud calendar, Librus timetables, Open-Meteo weather) rendered as
2-bit or 1-bit PNGs, plus a homelab placeholder. It ships as a multi-arch Docker image.

**Architecture:**
- Fetchers run as tokio tasks and write each source into a JSON-backed cache.
- Every `/api/display` request reads the caches and builds a pure view model, then
  renders it: MiniJinja HTML → Takumi (no browser) → quantize → PNG.
- The axum device API serves the protocol the TRMNL firmware expects.

**Tech Stack:**
- Rust 1.100.0, edition 2024, tokio, axum
- reqwest (rustls, cookies, multipart, json), scraper
- icalendar (parser) + rrule
- chrono + chrono-tz
- serde, toml
- minijinja (+ minijinja-embed, minijinja-autoreload)
- takumi `=2.22.1` (feature `from-html`), image, png
- clap, tracing
- tests: wiremock, tower

**Spec:** `docs/superpowers/specs/2026-11-25-family-dashboard-design.md` (and the
visual reference `docs/superpowers/specs/2026-11-25-family-dashboard-mockup.html`).

## Global Constraints

- **Names:** crate, binary and image are all `tablica`. The image is
  `ghcr.io/bwilczynski/tablica`.
- **Language:**
  - Every string drawn on a dashboard is Polish, and all of them come from
    `src/text/pl.rs`.
  - Code, comments, log messages, config keys, commits and docs are English.
- **Time zone:** all wall-clock logic runs in the configured zone (default
  `Europe/Warsaw`). Hours print without a leading zero (`8:00`, `16:30`). The en dash
  `–` separates time ranges and ` · ` separates parts.
- **Output:** 800×480. 2-bit means levels exactly 0, 85, 170 and 255. 1-bit means a
  threshold at 160. PNG at most **90,000 bytes**, served as `image/png`.
- **Tones:** light gray `#aaa` (170) marks structural areas (header band, day bars).
  Black is for text, rules and alerts. No other colors appear in templates.
- **Dependencies:**
  - `takumi = { version = "=2.22.1", features = ["from-html"] }`, pinned exactly
  - no browser, no Node, no database
- **Polling limits:**
  - Librus: at most every 2 h, in slots 05:30–21:30, plus one run at startup
  - calendar: every 15 min
  - weather: every 30 min
- **Failure policy:** the screen never shows an error or stack trace. A failing source
  keeps its last good data. Stale thresholds: calendar 2 h, timetable 26 h, weather 3 h.
- **Never send `reset_firmware: true`**, under any circumstances. It wipes the device's
  WiFi settings.
- **Commits:** Conventional Commits. No AI attribution trailers.
- **Development:** no Librus logins during development. Librus code is built and
  tested against mock servers and the fixtures the owner captures.

## Review Focus

1. **Librus serves a login or error page instead of a timetable** (expired session, wrong
   password, maintenance). Expected: a fetch error that keeps the cached timetable. It
   must not be read as an empty week, which would show "Brak lekcji". Covered by Task 7,
   `parse_week_rejects_login_page`.
2. **DST transitions** (last Sunday of March and of October). Expected: the night sleep
   still wakes the device at 06:00 local, and the Librus slots stay at local times.
   Covered by Task 11, `refresh_rate_across_dst_wakes_at_local_six`, and Task 8,
   `librus_slots_follow_local_time_across_dst`.
3. **Multi-day and midnight-crossing events.** Expected: a 3-day all-day event appears
   under each of its days inside the window, and a timed event from 22:00 to 01:00 shows
   once, on its start day. Covered by Task 5, `multi_day_all_day_event_listed_each_day`
   and `overnight_event_listed_on_start_day`.
4. **ICS times in UTC (`Z`), with a `TZID` or floating.** Expected: all three show in
   Warsaw local time. Floating times are taken as local. Covered by Task 5,
   `utc_tzid_and_floating_times_convert_to_local`.
5. **Worst-case content volume:** 9 lessons with long subject names, 10+ events, and
   long titles. Expected: the image still renders at 800×480 and stays ≤ 90,000 bytes;
   overflow becomes "+N więcej" or gets ellipsized. Covered by Task 9,
   `calendar_overflow_collapses_to_more_line`, and Task 10,
   `worst_case_family_render_fits_size_limit`.

---

## File structure

```
Cargo.toml  mise.toml  rust-toolchain.toml  .gitignore
config.sample.toml                      # documented example config (placeholder values only)
assets/fonts/Inter.ttf, assets/fonts/OFL.txt
templates/family.html  templates/homelab.html  templates/icons/{clear,partly-cloudy,cloudy,fog,rain,snow,storm}.svg
src/main.rs            # clap CLI: `serve`, `capture librus`
src/lib.rs             # module tree
src/config.rs          # TOML + secrets → Config
src/text/pl.rs         # every on-screen Polish string and date format
src/cache.rs           # Stamped<T>, SourceCache<T>, Caches, staleness
src/sources/weather.rs # Open-Meteo fetch + parse
src/sources/calendar.rs# ICS fetch (ETag) + parse/expand
src/sources/librus/{mod.rs,auth.rs,timetable.rs,capture.rs}
src/scheduler.rs       # fetcher loops, Librus slots, backoff
src/dashboards/{mod.rs,family.rs,homelab.rs}
src/render/{mod.rs,quantize.rs}
src/device_api/{mod.rs,refresh.rs,images.rs}
tests/fixtures/{weather,ics,librus}/…
contrib/docker/{Dockerfile,docker-compose.yaml}
.github/workflows/{ci.yml,image.yml}
```

Unit tests sit next to the code (`#[cfg(test)] mod tests`). HTTP flows use `wiremock`,
and the router is tested with `tower::ServiceExt::oneshot`.

---

### Task 1: Scaffold and config loading

**Files:**
- Create: `Cargo.toml`, `mise.toml`, `rust-toolchain.toml`, `.gitignore` (extend),
  `src/main.rs`, `src/lib.rs`, `src/config.rs`, `config.sample.toml`

**Interfaces:**
- Produces:
  - `config::load(path: &Path, secrets_dir: &Path) -> Result<Config, ConfigError>`
  - `Config { timezone: chrono_tz::Tz, base_url: String, listen: SocketAddr, data_dir: PathBuf, schedule: Schedule, devices: Vec<Device>, family: FamilyConfig }`
  - `Schedule { wake: NaiveTime, sleep: NaiveTime, refresh_minutes: u32 }`
  - `Device { name: String, mac: String /* uppercase, colon-separated */, api_key: String, dashboard: DashboardKind, bit_depth: BitDepth }`
  - `enum DashboardKind { Family, Homelab }`, `enum BitDepth { One, Two }`
  - `FamilyConfig { switch_at: NaiveTime, calendar_days: u32, ics_url: String, kids: Vec<KidConfig>, weather: WeatherConfig }`
  - `KidConfig { label: String, librus_login: String, librus_password: String }`
  - `WeatherConfig { latitude: f64, longitude: f64 }`

- [ ] **Step 1: Toolchain and crate.**
  - `mise.toml` pins `rust = { version = "1.100.0", profile = "minimal", components = ["clippy","rustfmt","rust-analyzer"] }`, with tasks `build`, `test` (`cargo test`) and `lint` (`cargo clippy --all-targets -- -D warnings && cargo fmt --check`).
  - `rust-toolchain.toml` mirrors 1.100.0.
  - `Cargo.toml` has `name = "tablica"`, edition 2024, `rust-version = "1.100"`, license MIT.
  - `.gitignore` adds `/target`, `.superpowers/`, `.env` and `/data`.
- [ ] **Step 2: Write the failing tests** in `src/config.rs`. Each test writes a temp config and a temp secrets dir.
  - `loads_sample_config`: parses `config.sample.toml` with the secrets present.
    - It asserts `timezone == Europe::Warsaw` and two devices.
    - The `family` device has `BitDepth::Two` (the default when `bit_depth` is absent).
    - `switch_at == 16:00`, `calendar_days == 7` and `kids.len() == 2`.
  - `secret_file_wins_and_is_trimmed`: the file `librus_kid1_password` contains `"s3cret\n"`, so `kids[0].librus_password == "s3cret"`.
  - `env_form_reads_environment`: `librus_password_env = "TABLICA_TEST_PW"` with that env var set gives the env value.
  - `missing_secret_is_error_naming_key`: the error message contains `librus_kid1_password`.
  - `mac_is_normalized`: `"aa-bb-cc-dd-ee-01"` → `"AA:BB:CC:DD:EE:01"`. An invalid MAC is an error.
  - `defaults_apply`: `listen` defaults to `0.0.0.0:8080`, `data_dir` to `/data`, `schedule` to 06:00/22:00/15, and `webcal://x` in `ics_url` becomes `https://x`.
- [ ] **Step 3: Run** `cargo test config` → FAIL (nothing is defined yet).
- [ ] **Step 4: Implement `config.rs`.**
  - Raw serde structs mirror the spec's TOML. Each secret field accepts `*_secret` (a file name under `secrets_dir`) or `*_env`.
  - Every resolved value is trimmed.
  - `config.sample.toml` is the spec's example with placeholder MACs and coordinates.
  - `main.rs` only parses `tablica serve --config <path> [--secrets-dir /run/secrets] [--templates <dir>]` for now and prints the device names it loaded.
- [ ] **Step 5: Run** `cargo test config` and `mise run lint` → PASS.
- [ ] **Step 6: Commit** `feat: scaffold tablica crate and config loading`

---

### Task 2: Polish text module

**Files:** Create `src/text/mod.rs` and `src/text/pl.rs`.

**Interfaces:**
- Produces, all in `text::pl`:
  - `date_header(d: NaiveDate) -> String`
  - `day_label(d: NaiveDate, today: NaiveDate) -> String`: "Dziś", "Jutro", otherwise `"czw. 26"`
  - `day_short(d: NaiveDate, today: NaiveDate) -> String`: "Dziś", "Jutro", otherwise `"czw."`
  - `hm(t: NaiveTime) -> String`
  - `range(a: NaiveTime, b: NaiveTime) -> String`: `"8:00–14:25"`
  - `temp(c: f32) -> String`
  - `high_low(max: f32, min: f32) -> String`
  - `condition(c: Condition) -> &'static str`
  - `precip_note(kind: PrecipKind, prob: u8, at: NaiveTime) -> String`
  - `more(n: usize) -> String`
  - `stale(at: DateTime<Tz>, today: NaiveDate) -> String`
  - consts `CALENDAR_HEADING`, `ALL_DAY`, `NO_LESSONS`, `CALENDAR_UNAVAILABLE`, `TIMETABLE_UNAVAILABLE`, `WEATHER_UNAVAILABLE`, `HOMELAB_PLACEHOLDER`, `TOMORROW_PREFIX`
- `Condition` and `PrecipKind` are defined here, in `text/mod.rs`, so that sources and views share them:
  - `enum Condition { Clear, PartlyCloudy, Cloudy, Fog, Rain, Snow, Storm }`
  - `enum PrecipKind { Rain, Snow }`

- [ ] **Step 1: Write the failing tests** (`text::pl::tests`):
  - `date_header_uses_genitive`: 2026-11-25 → `"Środa, 25 listopada"`. All 12 genitive months: stycznia, lutego, marca, kwietnia, maja, czerwca, lipca, sierpnia, września, października, listopada, grudnia.
  - `day_labels`: today → "Dziś", +1 → "Jutro", 2026-11-26 seen from 2026-11-24 → `"czw. 26"`. The short weekdays are `pon. wt. śr. czw. pt. sob. niedz.`
  - `times`: `hm(08:00)=="8:00"`, `range(8:00,14:25)=="8:00–14:25"`
  - `temps`: `temp(4.4)=="4°"`, `temp(-0.4)=="0°"` (no "-0"), `temp(-1.6)=="-2°"`, `high_low(6.0,1.0)=="maks. 6° min. 1°"`
  - `conditions`: Clear "Bezchmurnie", PartlyCloudy "Częściowe zachmurzenie", Cloudy "Pochmurno", Fog "Mgła", Rain "Deszcz", Snow "Śnieg", Storm "Burza"
  - `notes`: `precip_note(Rain,30,15:00)=="deszcz 30% od 15:00"`, `precip_note(Snow,40,9:00)=="śnieg 40% od 9:00"`, `more(3)=="+3 więcej"`, `stale(Mon 2026-11-23 07:30, today 2026-11-25)=="stan na pon. 7:30"`; when the stale time falls on today, the result is `"stan na 7:30"`
  - consts: `"TEN TYDZIEŃ"`, `"cały dzień"`, `"Brak lekcji"`, `"Kalendarz niedostępny"`, `"Plan lekcji niedostępny"`, `"Pogoda niedostępna"`, `"Homelab: wkrótce"`, `TOMORROW_PREFIX == "jutro: "`
- [ ] **Step 2: Run** `cargo test text` → FAIL.
- [ ] **Step 3: Implement** with plain lookup tables. Don't use chrono locale formatting, because it doesn't produce the genitive month forms.
- [ ] **Step 4: Run** `cargo test text` → PASS.
- [ ] **Step 5: Commit** `feat: add Polish on-screen text module`

---

### Task 3: Source cache

**Files:** Create `src/cache.rs`.

**Interfaces:**
- Produces:
  - `struct Stamped<T> { data: T, fetched_at: DateTime<Utc> }`, with Serialize, Deserialize and Clone
  - `struct SourceCache<T>`:
    - `fn open(path: PathBuf) -> Self`: loads the file if present; a corrupt file is logged and ignored
    - `fn get(&self) -> Option<Stamped<T>>`
    - `fn put(&self, data: T, at: DateTime<Utc>) -> io::Result<()>`: atomic write via a temp file and rename
    - `fn record_error(&self, msg: String, at: DateTime<Utc>)`
    - `fn last_error(&self) -> Option<(DateTime<Utc>, String)>`
  - `fn is_stale(fetched_at: DateTime<Utc>, now: DateTime<Utc>, max_age: chrono::Duration) -> bool`
  - consts `CALENDAR_MAX_AGE = 2h`, `TIMETABLE_MAX_AGE = 26h`, `WEATHER_MAX_AGE = 3h`
  - `struct Caches { calendar: SourceCache<IcsSnapshot>, weather: SourceCache<Weather>, timetables: Vec<SourceCache<Timetable>> }`
    - `Caches::open(data_dir: &Path, kids: usize) -> Caches`
    - files: `calendar.json`, `weather.json`, `timetable-kid{N}.json` (N from 1)
  - `IcsSnapshot`, `Weather` and `Timetable` come from Tasks 5, 4 and 7. Until then, use a placeholder `struct` in each source module with a TODO pointing at its task. Those tasks replace the placeholders.

- [ ] **Step 1: Write the failing tests:**
  - `put_then_reopen_returns_data`
  - `corrupt_file_yields_none`
  - `put_does_not_leave_temp_files`
  - `stale_boundaries`: exactly `max_age` is not stale, `max_age + 1s` is stale
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement** with a `std::sync::RwLock` inside and `serde_json` on disk. **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat: add JSON-backed source cache`

---

### Task 4: Weather source (Open-Meteo)

**Files:** Create `src/sources/mod.rs`, `src/sources/weather.rs`, and `tests/fixtures/weather/forecast.json` (a real response, saved once from the URL below with the sample coordinates).

**Interfaces:**
- Produces:
  - `struct Weather { current_temp_c: f32, current: Condition, today: Daily, tomorrow: Daily, hourly: Vec<HourlyPrecip> }`
  - `struct Daily { max_c: f32, min_c: f32, condition: Condition }`
  - `struct HourlyPrecip { at: NaiveDateTime /* local */, probability: u8, kind: PrecipKind }`
  - `fn condition_from_wmo(code: u16) -> Condition`
  - `fn parse(json: &str) -> Result<Weather, SourceError>`
  - `async fn fetch(client: &reqwest::Client, base_url: &str, cfg: &WeatherConfig, tz: Tz) -> Result<Weather, SourceError>`
  - `enum SourceError` (in `sources/mod.rs`): `Http`, `Status(u16)`, `Parse(String)`, `Auth(String)`, `NotTimetable`
- Request: `GET {base_url}/v1/forecast?latitude=…&longitude=…&current=temperature_2m,weather_code&daily=weather_code,temperature_2m_max,temperature_2m_min&hourly=precipitation_probability,weather_code&timezone=<tz name>&forecast_days=2`. The production `base_url` is `https://api.open-meteo.com`.

- [ ] **Step 1: Write the failing tests:**
  - `wmo_mapping`:
    - 0 → Clear; 1, 2 → PartlyCloudy; 3 → Cloudy; 45, 48 → Fog
    - 51–67 and 80–82 → Rain; 71–77, 85 and 86 → Snow; 95–99 → Storm
    - an unknown code → Cloudy
  - `parses_fixture`: two daily entries, 48 hourly entries, and the current temperature equals the fixture's value
  - `precip_kind_from_hourly_code`: an hour with code 73 → `PrecipKind::Snow`
  - `fetch_hits_expected_query` (wiremock): asserts the query params above and returns the fixture
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat: add Open-Meteo weather source`

---

### Task 5: iCloud calendar source (ICS)

**Files:** Create `src/sources/calendar.rs` and `tests/fixtures/ics/*.ics` (hand-written, with fictional events).

**Interfaces:**
- Produces:
  - `struct IcsSnapshot { text: String, etag: Option<String> }`
  - `enum FetchOutcome { NotModified, Fresh(IcsSnapshot) }`
  - `async fn fetch(client: &reqwest::Client, url: &str, etag: Option<&str>) -> Result<FetchOutcome, SourceError>`: sends `If-None-Match`, and 304 → `NotModified`
  - `struct CalendarEvent { title: String, when: EventWhen }`
  - `enum EventWhen { AllDay { first: NaiveDate, last: NaiveDate /* inclusive */ }, Timed { start: DateTime<Tz>, end: DateTime<Tz> } }`
  - `fn events_in(ics: &str, tz: Tz, from: NaiveDate, to: NaiveDate /* inclusive */) -> Result<Vec<CalendarEvent>, SourceError>`: sorted by start, with all-day events first within a day
- The cache stores the raw `IcsSnapshot`, and expansion happens at view time, so a `NotModified` response never leaves the window out of date.

- [ ] **Step 1: Write the failing tests:**
  - `single_timed_event`
  - `weekly_rrule_expands_in_window`: a weekly event shows up exactly in the weeks inside a 7-day window
  - `exdate_removes_occurrence`
  - `recurrence_id_override_replaces_occurrence`: it gets the new time and title
  - `utc_tzid_and_floating_times_convert_to_local`: `DTSTART:20261125T150000Z` → 16:00 Warsaw; `DTSTART;TZID=America/New_York:20261125T100000` → 16:00; a floating `20261125T160000` → 16:00
  - `multi_day_all_day_event_listed_each_day`: `DTSTART;VALUE=DATE:20261126` and `DTEND;VALUE=DATE:20261129` → `AllDay{first: 26, last: 28}` (DTEND is exclusive)
  - `overnight_event_listed_on_start_day`: 22:00 → 01:00 gives one `Timed` event that starts on the first day
  - `fetch_sends_if_none_match_and_handles_304` (wiremock)
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** with `icalendar` (feature `parser`) for parsing and the `rrule` crate for expansion. `VTIMEZONE` definitions are ignored: IANA `TZID`s go through `chrono-tz`, and an unknown TZID falls back to the configured zone with a `warn!`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat: add ICS calendar source with recurrence expansion`

---

### Task 6: Librus login and the `capture` command

**Files:** Create `src/sources/librus/mod.rs`, `src/sources/librus/auth.rs`, `src/sources/librus/capture.rs`. Modify `src/main.rs` to add the `capture librus` subcommand.

**Interfaces:**
- Produces:
  - `struct LibrusUrls { synergia: Url, api: Url, portal: Url }`, with a `Default` pointing at production: `https://synergia.librus.pl`, `https://api.librus.pl`, `https://portal.librus.pl`
  - `struct LibrusClient`. It owns a `reqwest::Client` with a cookie store, redirects disabled, and a Chrome-like User-Agent. One client is created per kid.
    - `fn new(urls: LibrusUrls) -> Self`
    - `async fn login(&self, login: &str, password: &str, trace: Option<&mut Vec<Exchange>>) -> Result<(), SourceError>`
    - `async fn timetable_page(&self, week: Option<&str>) -> Result<String, SourceError>`. `None` sends `GET {synergia}/przegladaj_plan_lekcji`. `Some(v)` sends `POST` as multipart with `tydzien=v`.
  - `struct Exchange { method: String, url: String, status: u16, headers: Vec<(String,String)>, body: String }`, recorded for the capture command
  - `fn x_baner(rng_value: u64, now_ms: u64) -> String`
  - `async fn capture(cfg: &Config, out: &Path) -> anyhow::Result<()>`
- Login steps, exactly as spec §Librus:
  1. `GET {synergia}/loguj/portalRodzina` with `Referer: {portal}/`
  2. `GET` its Location, then `GET` the next Location (the login form)
  3. `POST` the login form urlencoded with `action=login`, `login`, `pass`, plus the headers `x-baner`, `X-Requested-With: XMLHttpRequest`, `Accept: application/json, text/javascript, */*; q=0.01`, `Referer: <login url>` and `Origin: {api}`
  4. Follow `Location` or JSON `goTo` for at most 10 hops. Each URL must be on `{api}/OAuth/Authorization…` or `{synergia}/loguj/portalRodzina…`.
  5. A redirect to `{synergia}/rodzic/…` or `{synergia}/uczen/…`, or a 2xx response from an allowed URL, means success.
  6. Anything else → `SourceError::Auth("unexpected redirect: <host><path>")`.

  Hosts are compared against `LibrusUrls`, which lets tests aim at wiremock.

- [ ] **Step 1: Write the failing tests** (wiremock serves all three hosts on distinct path prefixes, using three `MockServer`s):
  - `x_baner_shifts_digits_by_20`: each character shifted +20 code points, joined with `_`. For `x_baner(12, 3)` that's `"EF_G"`, since `'1'+20='E'`, `'2'+20='F'` and `'3'+20='G'`.
  - `login_follows_chain_and_ends_on_rodzic_redirect`
  - `login_ends_on_200_from_portal_rodzina`
  - `login_accepts_goto_json_continuation`
  - `login_rejects_foreign_host`
  - `login_stops_after_10_hops`
  - `post_carries_required_headers_and_form`: the mock matcher asserts the headers and urlencoded body above
  - `capture_never_writes_password`: runs `capture` against the mocks and asserts that no output file contains the password string or any `Set-Cookie` value. Cookie values are written as `<redacted>`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** `auth.rs`, then `capture.rs`, and wire up `tablica capture librus --config <path> [--secrets-dir …] --out <dir>`. For each kid, capture:
  - logs in once with `trace`
  - fetches the current-week page, then the next week using the `<select name="tydzien">` option that follows the selected one
  - writes `kid{N}/NN-<method>-<host>.txt` (status, headers, body) and `kid{N}/timetable-current.html`, `timetable-next.html`
  - replaces the login with `LOGIN`
  - after Task 7 lands, also replaces teacher names with `Nauczyciel N`, using `timetable::parse_week` (best effort)
  - prints a reminder that the owner should review the files before committing
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat: add Librus login flow and capture command`

### ⏸ Owner checkpoint: capture real fixtures

The owner runs `tablica capture librus --config … --out tests/fixtures/librus/`, reviews
the files and commits them. Tasks 7–8 can start beforehand using the hand-written
fixture. Task 7's `owner_fixtures_parse` test is skipped until those files exist.

---

### Task 7: Librus timetable parsing and fetch

**Files:** Create `src/sources/librus/timetable.rs` and `tests/fixtures/librus/handmade/week.html`. The hand-written page follows the structure described below and uses fictional names.

**Interfaces:**
- Consumes: `LibrusClient::{login, timetable_page}` (Task 6).
- Produces:
  - `struct Lesson { start: NaiveTime, end: NaiveTime, subject: String, teacher: Option<String>, room: Option<String>, cancelled: bool, flag: Option<String> }`
  - `struct Timetable { weeks: Vec<NaiveDate /* Mondays covered */>, days: BTreeMap<NaiveDate, Vec<Lesson>> }`: only days that have lessons are present
  - `fn parse_week(html: &str) -> Result<(NaiveDate /* Monday */, BTreeMap<NaiveDate, Vec<Lesson>>), SourceError>`
  - `fn week_options(html: &str) -> Vec<(String /* value */, bool /* selected */)>`
  - `async fn fetch_two_weeks(client: &LibrusClient, login: &str, password: &str) -> Result<Timetable, SourceError>`
- Parsing rules (spec §Librus):
  - Rows: `table.plan-lekcji tr.line1`. Skip `line0` rows.
  - Row header `th`: the first two `H:MM` or `HH:MM` matches give the start and end.
  - Cells: `td` elements, with the first and last dropped; Monday is index 0.
  - In each `.text` element, `<b>` holds the subject. After the first `<br>`, the text splits on `" s. "` into teacher and room.
  - Ancestor `<s>` → `cancelled`. `.plan-lekcji-info` text, joined with `", "` → `flag`.
  - The Monday date comes from the selected `tydzien` option, whose value is `YYYY-MM-DD_YYYY-MM-DD`.

- [ ] **Step 1: Write the failing tests:**
  - `parses_handmade_week`: lesson counts per day, times, one cancelled lesson, one lesson flagged `"zastępstwo"`, and teacher and room split correctly
  - `parse_week_rejects_login_page`: HTML with no `table.plan-lekcji` → `Err(SourceError::NotTimetable)`
  - `empty_week_is_ok_and_empty`: the table is present with no lessons → `Ok` with an empty map. That's a holiday, not an error.
  - `week_options_finds_selected_and_next`
  - `owner_fixtures_parse`: for every `tests/fixtures/librus/kid*/timetable-*.html`, `parse_week` succeeds. If the directory is missing, it prints a skip message and passes.
  - `fetch_two_weeks_merges` (wiremock): the current page plus a POST with `tydzien=<next option>` → `weeks.len() == 2`
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement** with `scraper`. **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat: parse Librus timetable and fetch two weeks`

---

### Task 8: Fetch scheduler

**Files:** Create `src/scheduler.rs`.

**Interfaces:**
- Consumes: `Caches` (Task 3), `weather::fetch` (Task 4), `calendar::fetch` (Task 5), `timetable::fetch_two_weeks` (Task 7).
- Produces:
  - `fn next_librus_slot(now: DateTime<Tz>) -> DateTime<Tz>`: the slots are 05:30, 07:30, …, 21:30 local; after 21:30 it returns tomorrow's 05:30
  - `fn backoff_delay(attempt: u32) -> Duration`: 5 min × 2^attempt, capped at 60 min
  - `struct Sources { http: reqwest::Client, weather_base: String, librus: LibrusUrls }`
  - `fn spawn_all(cfg: Arc<Config>, caches: Arc<Caches>, sources: Sources, shutdown: CancellationToken) -> Vec<JoinHandle<()>>`
- Behavior:
  - Every loop runs once at startup.
  - The calendar loop runs every 15 min, keeps the stored ETag, and leaves the data untouched on 304 (the timestamp is still refreshed).
  - Weather runs every 30 min.
  - Librus runs once per kid, at slots. On failure it calls `record_error` and retries after `backoff_delay(n)`, but never past the next slot.
  - Every error is logged with `warn!` and never panics.

- [ ] **Step 1: Write the failing tests:**
  - `librus_slots`: 05:00 → 05:30, 05:30 → 07:30, 21:31 → next day 05:30
  - `librus_slots_follow_local_time_across_dst`: on 2026-03-29, starting at 01:00 local, the next slot is 05:30 local (+02:00)
  - `backoff_caps_at_60_min`
  - `calendar_304_keeps_data_and_refreshes_timestamp`: calls a `refresh_calendar_once(...)` helper against wiremock
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement**, with each loop as `tokio::select!` on sleep and the cancellation token. **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat: schedule source fetchers`

---

### Task 9: Family view model

**Files:** Create `src/dashboards/mod.rs`, `src/dashboards/family.rs`, `src/dashboards/homelab.rs`.

**Interfaces:**
- Consumes: `text::pl` (Task 2), `Stamped`/`is_stale` (Task 3), `Weather` (Task 4), `events_in` (Task 5), `Timetable`/`Lesson` (Task 7).
- Produces. Every struct derives `Serialize`, and each field is a final display string:
  - `struct FamilyView { header: Header, calendar: CalendarCol, kids: Vec<KidCol> }`
  - `struct Header { date: String, temp: Option<String>, condition: Option<String>, icon: Option<&'static str>, detail: Option<String>, note: Option<String> }`
    - `icon` is one of `clear|partly-cloudy|cloudy|fog|rain|snow|storm`
    - `note` holds the stale or unavailable text
  - `struct CalendarCol { heading: &'static str, days: Vec<CalDay>, more: Option<String>, note: Option<String> }`
  - `struct CalDay { label: String, events: Vec<EventLine> }`
  - `struct EventLine { time: String, title: String }`
  - `struct KidCol { label: String, featured: Option<Featured>, summary: Option<Summary>, empty: Option<&'static str>, note: Option<String> }`
  - `struct Featured { title: String, lessons: Vec<LessonLine> }`
  - `struct LessonLine { time: String, subject: String, cancelled: bool, flag: Option<String> }`
  - `struct Summary { title: String, subjects: Option<String> }`
  - `struct FamilyInputs<'a> { now: DateTime<Tz>, cfg: &'a FamilyConfig, calendar: Option<&'a Stamped<IcsSnapshot>>, weather: Option<&'a Stamped<Weather>>, timetables: Vec<Option<&'a Stamped<Timetable>>> }`
  - `fn build(inputs: &FamilyInputs) -> FamilyView`
  - `fn next_school_day(tt: &Timetable, after: NaiveDate) -> Option<NaiveDate>`: the first day strictly after `after`
  - `const MAX_CALENDAR_EVENTS: usize = 10`
  - `struct HomelabView { message: &'static str }` and `fn homelab::build() -> HomelabView`
- Rules (spec §Time rules):
  - `after_switch = now.time() >= cfg.switch_at`.
  - Before the switch: featured = today if it has lessons, otherwise `next_school_day(today)`, and summary = `next_school_day(featured)` with the subject list.
  - After the switch: featured = `next_school_day(today)`, and summary = today with times only (`title: "Dziś · 8:00–14:25"`, `subjects: None`), or nothing if today had no lessons.
  - Titles are `"<day_short> · <range>"`, where the range runs from the first lesson's start to the last lesson's end (cancelled lessons included).
  - The subject list is joined with `", "` and consecutive repeats collapse to `"X ×2"`.
  - No featured day → `empty = Some(NO_LESSONS)`.
  - Calendar: the window runs from today to today + `calendar_days` − 1. Timed events that have ended are dropped. All-day events get the time `"cały dzień"`. After `MAX_CALENDAR_EVENTS` events, the rest collapse into `more`.
  - Header detail:
    - before the switch: `high_low(today)` plus `" · " + precip_note(...)` for the first hour from the current hour to 23:00 with probability ≥ 30
    - after the switch: `TOMORROW_PREFIX + high_low(tomorrow) + " · " + condition(tomorrow).to_lowercase()`
  - A stale source sets `note = stale(fetched_at)`. A missing source sets `note` to its `*_UNAVAILABLE` const and its data fields to `None` or empty.

- [ ] **Step 1: Write the failing tests.** Fixtures are built in code with Warsaw times, and 2026-11-25 is a Wednesday.
  - `before_switch_features_today_and_summarizes_next_day`: at 07:00, the title is `"Dziś · 8:00–14:25"` and the summary title is `"czw. · 8:55–13:30"`
  - `after_switch_features_next_day_and_shows_today_times`: at 19:00, `"Jutro · 8:55–13:30"` plus a summary `"Dziś · 8:00–14:25"` with `subjects == None`
  - `friday_evening_features_monday`: Fri 2026-11-27 at 19:00 → `"pon. · …"`
  - `holiday_week_shows_no_lessons`: an empty timetable → `empty == Some("Brak lekcji")`
  - `no_lessons_today_morning_features_next_school_day`
  - `repeats_collapse`: `["Ed. wczesnoszkolna","Ed. wczesnoszkolna","Muzyka"]` → `"Ed. wczesnoszkolna ×2, Muzyka"`
  - `cancelled_and_flag_pass_through`
  - `calendar_groups_days_and_drops_ended_events`: at 17:00, the 16:30–17:30 event is kept and the 15:00–16:00 one is dropped
  - `calendar_overflow_collapses_to_more_line`: 13 events → 10 lines plus `more == Some("+3 więcej")`
  - `weather_detail_before_and_after_switch`
  - `stale_and_missing_sources_set_notes`: a weather timestamp 4 h old → `"stan na …"`; no calendar → `"Kalendarz niedostępny"`
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat: build family dashboard view model`

---

### Task 10: Renderer and templates

**Files:**
- Create: `src/render/mod.rs`, `src/render/quantize.rs`
- Create: `templates/family.html`, `templates/homelab.html`, `templates/icons/*.svg`
- Create: `assets/fonts/Inter.ttf`, which is the Google Fonts `ofl/inter/Inter[opsz,wght].ttf`, plus its `OFL.txt`

**Interfaces:**
- Consumes: `FamilyView`, `HomelabView` (Task 9), `BitDepth` (Task 1).
- Produces:
  - `enum TemplateSource { Embedded, Dir(PathBuf) }`. `Dir` uses `minijinja-autoreload`; `Embedded` uses `minijinja-embed`.
  - `struct Renderer`, with `Send + Sync` and fonts registered once:
    - `fn new(src: TemplateSource) -> anyhow::Result<Renderer>`
    - `fn render<S: Serialize>(&self, template: &str, ctx: &S, depth: BitDepth) -> anyhow::Result<Vec<u8>>`
  - `fn quantize(rgba: &image::RgbaImage, depth: BitDepth) -> image::GrayImage`: composites onto white, uses Rec.601 luma, then gives 4 levels {0, 85, 170, 255} or a threshold at 160
  - `fn encode_png(img: &image::GrayImage, depth: BitDepth) -> Vec<u8>`: the `png` crate, `ColorType::Grayscale`, with `BitDepth::Two` or `BitDepth::One` packing
  - `const MAX_PNG_BYTES: usize = 90_000`. When it's exceeded, `render` logs `error!` and still returns the PNG.
- Template:
  - Port the CSS and structure from the spec's mockup HTML, with variables substituted.
  - Icons are inlined via `{% include "icons/" ~ header.icon ~ ".svg" %}`.
  - Each kid column uses a width of `(800-280)/kids|length`.
  - The all-day "cały dzień" time cell must not push titles out of line, so give `.t` a fixed width that fits the widest label.
- The Takumi calls are in spec §Rendering pipeline, as a code block.

- [ ] **Step 1: Write the failing tests:**
  - `quantize_levels_2bit`: luma 40 → 0, 100 → 85, 150 → 170 and 230 → 255; alpha 0 → 255
  - `quantize_threshold_1bit`: 159 → 0 and 160 → 255
  - `png_roundtrip_bit_depths`: decoding with the `png` crate reports `BitDepth::Two` or `One` for 800×480
  - `family_render_is_valid_png`: a `FamilyView` from the Task 9 test helpers renders to 800×480 at ≤ 90,000 bytes for both depths
  - `worst_case_family_render_fits_size_limit`: 9 lessons of `"Wychowanie fizyczne i zdrowotne"` per kid, 10 events with 60-character titles and every note set gives ≤ 90,000 bytes
  - `homelab_render_is_valid_png`
  - `embedded_and_dir_templates_match`: `Dir("templates")` and `Embedded` produce identical bytes
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Visual check.**
  - Write the 07:00 and 19:00 renders of the test view to `target/preview/`.
  - Compare them by eye with the mockup HTML: same structure, gray bands, the inverted `zast.` tag.
  - Note any differences in the commit body.
- [ ] **Step 6: Commit** `feat: render dashboards to 2-bit and 1-bit PNG`

---

### Task 11: Device API and `serve`

**Files:** Create `src/device_api/mod.rs`, `src/device_api/refresh.rs`, `src/device_api/images.rs`. Modify `src/main.rs` (`serve`).

**Interfaces:**
- Consumes: everything above. `scheduler::spawn_all` (Task 8), `Renderer` (Task 10), `family::build` and `homelab::build` (Task 9).
- Produces:
  - `fn refresh_rate(now: DateTime<Tz>, schedule: &Schedule, switch_at: Option<NaiveTime>) -> u32`, in seconds:
    - inside `[wake, sleep)`: `min(refresh_minutes×60, secs to switch_at+60s)` if that switch time falls within the window, otherwise `refresh_minutes×60`
    - outside: the seconds until the next local `wake`, resolved with `tz.from_local_datetime(..).earliest()`
    - never less than 60
  - `struct ImageStore`:
    - `fn put(&self, device: &str, dashboard: &str, png: Vec<u8>, now: DateTime<Utc>) -> String /* filename */`
    - `fn get(&self, filename: &str) -> Option<Vec<u8>>`
    - The filename is `"{dashboard}-{first 8 hex of sha256(png)}-{unix secs when this hash was first stored}"`, at most 31 characters, and identical for identical pixels.
    - It keeps the last 8 images per device.
  - `struct AppState { cfg: Arc<Config>, caches: Arc<Caches>, renderer: Arc<Renderer>, images: Arc<ImageStore>, clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync> }`
  - `fn router(state: AppState) -> axum::Router`
- Routes and responses (JSON numbers stay numbers):
  - `GET /api/setup`:
    - Header `ID` = MAC, matched case-insensitively.
    - Known MAC → 200 `{"status":200,"api_key","friendly_id","image_url","message":"tablica: <name>"}`. `friendly_id` is the first 6 uppercase hex characters of sha256(MAC).
    - Unknown MAC → HTTP 404 `{"status":404,"message":"unknown device"}`.
  - `GET /api/display`:
    - Header `Access-Token`. Unknown or missing → HTTP 401 `{"status":500,"error":"Device not found","reset_firmware":false}`.
    - Otherwise:
      1. render the device's dashboard at `clock()` in its own depth
      2. `ImageStore::put`
      3. reply 200 `{"status":0,"image_url":"{base_url}/images/{filename}.png","filename","refresh_rate","reset_firmware":false,"update_firmware":false,"firmware_url":null,"special_function":"none","maximum_compatibility":false,"temperature_profile":"default"}`
    - If rendering fails, re-serve that device's previous filename, and if there isn't one, return 503.
    - Log `Battery-Voltage`, `RSSI`, `FW-Version`, `Model`, `Width` and `Height` at `debug!`.
  - `POST /api/log`: logs each `logs[].message` at `info!` with the device name, then 204.
  - `GET /images/{filename}.png`: `image/png`, or 404.
  - `GET /preview/{family|homelab}.png?at=<RFC3339>&depth=1|2`: the same render path without storing. Default depth is 2.
  - `GET /healthz`: `{"status":"ok","sources":{"calendar":{"fetched_at","stale","last_error"},"weather":{…},"timetables":[…]}}`.
- `serve`:
  1. loads the config
  2. opens the caches under `data_dir`
  3. builds the `Renderer`, with `--templates <dir>` → `Dir`
  4. spawns the fetchers
  5. binds `listen`
  6. on SIGTERM or SIGINT (`tokio::signal::unix`), cancels the fetchers and shuts axum down gracefully

- [ ] **Step 1: Write the failing tests** (`oneshot` with a fixed clock and seeded caches):
  - `refresh_rate_daytime_is_15_min`
  - `refresh_rate_lands_after_switch`: at 15:50 → 660 (16:01)
  - `refresh_rate_night_sleeps_until_wake`: at 22:00 → 28,800
  - `refresh_rate_across_dst_wakes_at_local_six`: at 2026-10-24 22:00 local (+02:00) → 32,400, because the clocks fall back overnight and 06:00 is now +01:00
  - `setup_known_and_unknown_mac`
  - `display_rejects_bad_token_without_reset`: asserts `reset_firmware == false` and HTTP 401
  - `display_returns_numeric_refresh_rate_and_stable_filename`: two calls with the same data give the same `filename`, `refresh_rate` is a JSON number, and the filename is ≤ 31 characters
  - `display_serves_png_at_image_url`: `content-type: image/png`
  - `preview_honors_at_and_depth`
  - `log_returns_204`
  - `healthz_reports_staleness`
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Smoke test** with `cargo run -- serve --config config.sample.toml --secrets-dir <temp dir of dummy secrets> --templates templates`:
  - `curl -s localhost:8080/healthz` → `"status":"ok"`
  - `curl -s -o /tmp/f.png 'localhost:8080/preview/family.png?at=2026-11-25T19:00:00+01:00'`, then open it. The weather and calendar sections show "niedostępny" notes until the first fetch.
  - `kill -TERM <pid>` exits within 2 s.
- [ ] **Step 6: Commit** `feat: serve TRMNL device API`

---

### Task 12: Container and CI

**Files:** Create `contrib/docker/Dockerfile`, `contrib/docker/docker-compose.yaml`, `.dockerignore`, `.github/workflows/ci.yml`, `.github/workflows/image.yml`. Modify `README.md` with a short "Running" section.

**Interfaces:** Consumes `tablica serve` (Task 11).

- [ ] **Step 1: Dockerfile** (spec §Deployment):
  - `FROM rust:1.100-bookworm AS build`, `cargo build --release --locked`
  - then `FROM debian:bookworm-slim` with `ca-certificates` only
  - user `tablica`, uid 1000, `VOLUME /data`, `EXPOSE 8080`
  - `ENTRYPOINT ["/usr/local/bin/tablica"]`, `CMD ["serve","--config","/data/config.toml"]`
  - no tini
- [ ] **Step 2: compose:**
  - image `ghcr.io/bwilczynski/tablica:latest`, `restart: unless-stopped`
  - `ports: ["8080:8080"]`, `volumes: ["./data:/data"]`
  - `secrets:` for every `*_secret` in `config.sample.toml`, as files in `./secrets/`
- [ ] **Step 3: Workflows:**
  - `ci.yml`: fmt, clippy `-D warnings` and test on pushes and PRs
  - `image.yml`:
    - on PRs, an amd64 smoke build that runs the container with `config.sample.toml` plus dummy secrets and polls `curl -fsS localhost:8080/healthz`
    - on `v*` tags, an amd64 and arm64 build pushed by digest and merged into one manifest tagged `<version>` and `latest`, mirroring twydo's `image.yml`
- [ ] **Step 4: Verify locally:**
  - `docker build -f contrib/docker/Dockerfile -t tablica:dev .` succeeds
  - the image is under 120 MB (`docker image ls tablica:dev`)
  - the container serves `/healthz`
  - `docker stop` takes under 3 s
- [ ] **Step 5: Commit** `build: add container image, compose example and CI`

---

## Out of scope for this plan

- The homelab dashboard's content. It's only a placeholder here.
- Checks on real hardware: whether 2-bit PNGs are accepted, ghosting, and the Model header. The spec lists these under "Open questions".
- release-plz automation.
