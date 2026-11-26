# Dashboard Crates Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split tablica into a Cargo workspace (`tablica-core`, `tablica-family`, `tablica-homelab`, `tablica` binary) where dashboards plug into the TRMNL server through a trait, and switch releases from release-plz to release-please.

**Architecture:** The plug-in seam (`DashboardEntry` → `ParsedDashboard` → `Dashboard`) is introduced inside the current single crate first, so core code stops importing family code while everything is still one crate. Only then is the repo converted to a workspace and modules are moved into crates, one crate per task. The release switch ships first, independently.

**Tech Stack:** Rust 2024 (toolchain 1.100), axum 0.8, tokio, minijinja 2.24 + minijinja-embed, takumi, reqwest, release-please-action v5.0.0 (release-please 17.6.0).

**Spec:** `docs/superpowers/specs/2026-11-26-dashboard-crates-design.md` (revision 4). Read it first; this plan does not repeat its reasoning.

## Global Constraints

- Pure refactor: images, image filenames, refresh rates, previews, config format and validation, template override layout, cache file names stay the same. The only deliberate changes are the four in the spec's "Deliberate changes".
- Every commit builds and passes `mise run lint` and `mise run test`.
- Core code (`src/device_api/`, `src/render/`, `src/polling/`, `src/cache.rs`, `src/config.rs`, `src/dashboard.rs`, `src/server.rs`, later `crates/core/`) never imports family or homelab code, and never contains the word "family" outside `#[cfg(test)]` code.
- Poller start (`Dashboard::spawn_pollers`) happens only after the listener is bound.
- Family cache files stay `data_dir/calendar.json`, `data_dir/weather.json`, `data_dir/timetable-kid{N}.json` (N from 1).
- Dashboard names: `[a-z]{5,7}`, unique, not one of `timezone`, `base_url`, `listen`, `data_dir`, `schedule`, `device`.
- `/healthz` new shape: `{"status":"ok","dashboards":{"<name>": <Dashboard::health()>, ...}}`.
- No raw response text in errors, caches, `/healthz`, or logs at any level. `SourceError::CredentialsRejected` stays its own variant.
- Commits: Conventional Commits, no agent attribution trailers. Use `refactor:`/`test:`/`ci:`/`build:`/`docs:`/`chore:`; the error-text change is `fix:`.
- Tags stay `vX.Y.Z`; secret stays `RELEASE_PLZ_TOKEN`, passed via `with: token:`.
- Fixtures under `**/tests/fixtures/librus/kid*` are owner captures: never commit them, never let them into the Docker context.

## Review Focus

1. **A `[homelab]` section with arbitrary keys in an existing config** is ignored, not rejected (today unknown sections are ignored). Test `homelab_section_with_keys_is_ignored` in Task 3.
2. **`/api/setup` for each device** still returns `image_url` `{base}/preview/{dashboard}.png?depth={1|2}` with the device's dashboard name and depth. Test `setup_image_url_uses_dashboard_name_and_depth` in Task 5.
3. **SIGTERM while a poller is stuck in a fetch** still stops the process within the 1 s budget. Test `shutdown_with_hanging_poller_returns_within_budget` in Task 6.
4. **A fresh install with no cache files** reports every family source `stale: true`, `fetched_at: null` in `/healthz` without errors. Test `healthz_on_empty_caches_reports_stale` in Task 4.
5. **`/preview/<name>.png?at=...&depth=...`** keeps today's parsing (space as `+`, 400 on bad `at`/`depth`) for every dashboard, including a stub. Test `preview_query_parsing_applies_to_any_dashboard` in Task 5.

---

## File map (end state)

```
Cargo.toml                         virtual workspace
release-please-config.json, .release-please-manifest.json
.github/workflows/release.yml      replaces release-plz.yml
crates/core/src/{lib.rs, cache.rs, config.rs, dashboard.rs, server.rs,
                 polling/, render/, device_api/}
crates/family/{build.rs, templates/, tests/fixtures/}
crates/family/src/{lib.rs, config.rs, dashboard.rs, caches.rs, view.rs (was dashboards/family.rs),
                   test_support.rs, polling/, sources/, text/}
crates/homelab/{build.rs, templates/homelab.html, src/lib.rs}
crates/tablica/src/main.rs, crates/tablica/tests/end_to_end.rs
```

---

### Task 1: Switch releases to release-please (single crate)

Spec: "Release (release-please)", migration step 0. Native `rust` release type is fine here (one `[package]` at the root).

**Files:**
- Create: `release-please-config.json`, `.release-please-manifest.json`, `.github/workflows/release.yml`
- Delete: `release-plz.toml`, `.github/workflows/release-plz.yml`
- Modify: `CHANGELOG.md` (remove the empty `## [Unreleased]` heading), `README.md` (Contributing section: release-plz → release-please, recovery notes), `.github/workflows/pr-validation.yml` (comment mentioning release-plz)

- [ ] **Step 1: Write `release-please-config.json`**

```json
{
  "$schema": "https://raw.githubusercontent.com/googleapis/release-please/main/schemas/config.json",
  "include-component-in-tag": false,
  "bump-minor-pre-major": true,
  "bump-patch-for-minor-pre-major": true,
  "changelog-sections": [
    { "type": "feat", "section": "Added" },
    { "type": "fix", "section": "Fixed" },
    { "type": "perf", "section": "Performance" },
    { "type": "refactor", "hidden": true }, { "type": "chore", "hidden": true },
    { "type": "docs", "hidden": true }, { "type": "test", "hidden": true },
    { "type": "ci", "hidden": true }, { "type": "build", "hidden": true },
    { "type": "style", "hidden": true }
  ],
  "packages": { ".": { "release-type": "rust", "package-name": "tablica", "changelog-path": "CHANGELOG.md" } }
}
```

`.release-please-manifest.json`: `{ ".": "0.1.3" }`.

- [ ] **Step 2: Write `.github/workflows/release.yml`**

Triggered on push to `main`; job `ci` uses `./.github/workflows/ci.yml`; job `release` (needs `ci`, permissions `contents: write`, `pull-requests: write`, `issues: write`) runs `googleapis/release-please-action@v5.0.0` with `token: ${{ secrets.RELEASE_PLZ_TOKEN }}`, `config-file: release-please-config.json`, `manifest-file: .release-please-manifest.json`, and no `release-type` input. Comment above the step: the secret keeps its historical name; it is a fine-grained PAT (Contents + Pull requests read/write, possibly Issues read/write for labels) so that the tag it creates triggers `image.yml`.

- [ ] **Step 3: Delete release-plz files, update CHANGELOG/README/pr-validation comment**

- [ ] **Step 4: Prove release-PR content (offline gate)**

Ask the user before pushing anything. With their OK, push scratch branches `rp-proof/<case>` (each from `v0.1.3` plus this task's commit plus one synthetic commit) and run per case:
`npx release-please@17.6.0 release-pr --dry-run --token "$(gh auth token)" --repo-url bwilczynski/tablica --target-branch rp-proof/<case> --config-file release-please-config.json --manifest-file .release-please-manifest.json`

| Case | Synthetic commit | Expected dry-run output |
|---|---|---|
| feat-src | `feat: x` touching `src/lib.rs` comment | PR for `0.1.4`, one Added entry |
| fix-assets | `fix: y` touching `assets/fonts/OFL.txt` | PR for `0.1.4`, one Fixed entry |
| refactor | `refactor: z` | no PR |
| perf | `perf: p` | PR for `0.1.4`; with an added `fix:` still one PR |
| breaking | `refactor!: b` | PR for `0.2.0` |

Every case: the proposed changelog contains nothing at or before `4aa67e3` and `Cargo.toml`/`Cargo.lock` show the new version. Delete the scratch branches afterwards. Record outputs in the PR description. If a case fails and config cannot fix it, stop and report.

- [ ] **Step 5: Commit**

```bash
git add -A release-please-config.json .release-please-manifest.json .github/workflows CHANGELOG.md README.md
git rm release-plz.toml .github/workflows/release-plz.yml
git commit -m "ci: release with release-please instead of release-plz"
```

Tag/release/event behaviour is verified on the first real release (or in a disposable repo if the user authorizes one); note this as pending in the PR.

---

### Task 2: Pin today's cache format with a fixture

**Files:**
- Create: `tests/fixtures/cache/calendar.json`, `weather.json`, `timetable-kid1.json` (synthetic data, written by the current code)
- Modify: `src/family/mod.rs` (tests), `src/family/polling/timetable_policy.rs` (tests)

**Interfaces:**
- Produces: fixture dir `tests/fixtures/cache/` (moves with family in Task 9).

- [ ] **Step 1: Generate the fixtures with today's code**

Write a throwaway `#[test] #[ignore]` that puts `sample_inputs(warsaw(2026,11,25,7,0))` calendar/weather/kid-1 timetable into `FamilyCaches::open(tests/fixtures/cache, 1)`; run it once with `cargo test -- --ignored write_cache_fixture`, then delete the test. Commit only the JSON.

- [ ] **Step 2: Write the tests**

`family::tests::pre_split_cache_files_load_unchanged`: copy the fixture dir to a tempdir, `FamilyCaches::open(dir, 1)`; assert each `get()` is `Some`, `fetched_at` equals the literal timestamp in the JSON, and `data` equals the corresponding `sample_inputs` value.

`timetable_policy::tests::cached_fixture_takes_warm_start`: with caches opened from the fixture copy and `now` = the fixture's `fetched_at` + 1 h, `TimetablePolicy::first_run(now)` returns a time later than `now + 1 min` (not an immediate cold start).

- [ ] **Step 3: Run** `cargo test pre_split_cache cached_fixture` — expected PASS (they pin current behaviour).

- [ ] **Step 4: Commit** `test: pin the family cache file format before the crate split`

---

### Task 3: Plug-in contract and config split (single crate)

**Files:**
- Create: `src/dashboard.rs`, `src/family/config.rs`, `src/family/dashboard.rs` (entry + parse only in this task), `src/dashboards/homelab.rs` gains `entry()`
- Modify: `src/config.rs`, `src/lib.rs`, every user of `cfg.family` / `DashboardKind` (compile-driven: `device_api`, `scheduler`, `family/polling`, `sources/librus/capture.rs`, `dashboards/test_support.rs`, `main.rs`)

**Interfaces:**
- Produces (`src/dashboard.rs`):
  ```rust
  pub struct DashboardEntry {
      pub name: &'static str,
      pub parse: fn(Option<&toml::Value>, &Secrets) -> Result<Option<Box<dyn ParsedDashboard>>, ConfigError>,
  }
  pub trait ParsedDashboard: Any + Send {
      fn prepare(self: Box<Self>, ctx: &PrepareContext) -> anyhow::Result<Arc<dyn Dashboard>>;
  }
  pub struct PrepareContext<'a> { pub timezone: Tz, pub data_dir: &'a Path }
  pub trait Dashboard: Send + Sync {
      fn load_templates(&self, env: &mut minijinja::Environment<'static>);
      fn spawn_pollers(self: Arc<Self>, shutdown: CancellationToken) -> Vec<JoinHandle<()>>;
      fn view(&self, now: DateTime<Tz>) -> anyhow::Result<View>;
      fn switch_at(&self) -> Option<NaiveTime> { None }
      fn health(&self, now: DateTime<Utc>) -> serde_json::Value;
  }
  pub struct View { pub template: &'static str, pub context: minijinja::Value }
  pub fn validate_names(entries: &[DashboardEntry]) -> Result<(), ConfigError>;
  ```
  In this task `ParsedDashboard` is declared as `pub trait ParsedDashboard: Any + Send {}` (no methods) and `Dashboard`/`View`/`PrepareContext` are declared but unimplemented; Task 4 adds `prepare` to `ParsedDashboard` together with its implementations.
- Produces (`src/config.rs`):
  - `pub struct Secrets` with `pub fn new(dir: PathBuf) -> Secrets`, `pub fn resolve(&self, owner: &str, key: &str, file: Option<&str>, env: Option<&str>) -> Result<String, ConfigError>` (today's `resolve_secret` rules, unchanged), `pub fn reads(&self) -> usize` (count of `resolve` calls, `AtomicUsize`).
  - `Config { timezone, base_url, listen, data_dir, schedule, devices }` (no `family`); `Device.dashboard: String`; `DashboardKind` deleted.
  - `pub struct ParsedEntry { pub name: &'static str, pub parsed: Box<dyn ParsedDashboard> }`
  - `pub struct Loaded { pub config: Config, pub dashboards: Vec<ParsedEntry>, pub secrets: Secrets }`
  - `pub fn load(path: &Path, secrets_dir: &Path, entries: &[DashboardEntry]) -> Result<Loaded, ConfigError>`: `validate_names`, parse globals/devices, call each entry's `parse` with the top-level table of its name (if any), keep `Some` results, then reject a device whose `dashboard` is not among them.
  - New `ConfigError` variants: `Dashboard { section: String, message: String }`, `UnknownDashboard { device: String, dashboard: String }`, `InvalidDashboardName { name: String, reason: &'static str }`.
- Produces (`src/family/config.rs`): `FamilyConfig`, `KidConfig`, `WeatherConfig` (moved, with redacting `Debug`), `pub fn parse_config(section: Option<&toml::Value>, secrets: &Secrets) -> Result<FamilyConfig, ConfigError>` (absent section → `ConfigError::Dashboard { section: "family", message: "missing [family] section" }`; secrets resolved exactly as today incl. `webcal://` → `https://`).
- Produces (`src/family/dashboard.rs`): `pub struct ParsedFamily { pub cfg: FamilyConfig, pub endpoints: Endpoints }`, `pub struct Endpoints { pub weather_base: String, pub librus: LibrusUrls }` (`Default` = today's production URLs), `pub fn entry() -> DashboardEntry` (name `"family"`), `impl ParsedDashboard for ParsedFamily {}`.
- Produces (`src/dashboards/homelab.rs`): `pub fn entry() -> DashboardEntry` (name `"homelab"`, always `Some`, ignores the section).

Interim wiring for this task only: `main.rs` downcasts the family `ParsedEntry` (`Box<dyn ParsedDashboard>` → `Box<dyn Any>` via trait upcasting → `ParsedFamily`) and passes `FamilyConfig` to the existing `AppState`/scheduler, which take `Arc<FamilyConfig>` alongside `Arc<Config>`. `device_api` maps `device.dashboard` strings to its existing two render branches. Task 5 removes this.

- [ ] **Step 1: Write failing tests** (in `src/dashboard.rs`, `src/config.rs`, `src/family/config.rs`)

- `validate_names_rules`: `"family"`, `"homelab"`, `"abcde"` ok; `"abcd"`, `"abcdefgh"`, `"Family"`, `"fam1ly"` → `InvalidDashboardName`; duplicate names → error; `"device"`/`"listen"` → error (reserved).
- `missing_family_is_an_error_with_zero_devices` and `malformed_family_is_an_error_with_zero_devices` (e.g. `switch_at = 5`) → `Err`.
- `device_referencing_unregistered_dashboard_is_rejected` (`dashboard = "garden"`) → `UnknownDashboard`.
- `device_referencing_inactive_dashboard_is_rejected`: a test-only entry `"stubby"` whose `parse` returns `Ok(None)`; a device naming it → `UnknownDashboard`.
- `unknown_top_level_section_is_ignored` (`[weather_station] x = 1`) → `Ok`.
- `homelab_section_with_keys_is_ignored` (`[homelab] token = "x"`) → `Ok`, homelab active. *(Review Focus 1)*
- `sample_config_loads`: `config.sample.toml` with the existing secrets fixture → both `family` and `homelab` in `dashboards`, devices' `dashboard` strings `"family"`/`"homelab"`.
- Split secrets test: `devices_debug_never_contains_secrets` (core) and `family_debug_never_contains_secrets` (family). Secrets are unique substrings (`SECRETKEY-kitchen`, `SECRETICS`, `SECRETLOGIN1`, `SECRETPASS1`…); assert none of them appears in `{:?}`/`{:#?}` output (fixes the `webcal://` vs `https://` blind spot).
- Keep every other existing `config.rs` test, moved to whichever side owns the field.

- [ ] **Step 2: Run** `cargo test config dashboard` — expected FAIL (types missing).

- [ ] **Step 3: Implement** the interfaces above; fix compile errors across the crate with the interim wiring.

- [ ] **Step 4: Run** `mise run lint && mise run test` — expected PASS, all pre-existing tests green.

- [ ] **Step 5: Commit** `refactor: parse dashboard config sections through a plug-in entry list`

---

### Task 4: Family and homelab implement `Dashboard`

**Files:**
- Modify: `src/family/dashboard.rs`, `src/family/mod.rs`, `src/family/polling/mod.rs`, `src/scheduler.rs` (contents move into `src/family/refresh.rs`; delete `scheduler.rs`), `src/dashboards/homelab.rs`

**Interfaces:**
- Consumes: Task 3 contract.
- Produces:
  - `pub struct FamilyDashboard` (fields: `cfg: FamilyConfig`, `tz: Tz`, `caches: Arc<FamilyCaches>`, `http: reqwest::Client`, `endpoints: Endpoints`), built by `ParsedFamily::prepare`: opens `FamilyCaches::open(ctx.data_dir, kids)` and builds the client with 10 s connect / 30 s total timeouts (constants move from `main.rs`). Errors from client build propagate.
  - `FamilyDashboard` implements `Dashboard`: `view` = `dashboards::family::build` → `View { template: "family.html", context: minijinja::Value::from_serialize(&view) }`; `switch_at` = `Some(cfg.switch_at)`; `health` = `{"calendar":…,"weather":…,"timetables":[…]}` using `source_health` (moved to `src/cache.rs` as `pub fn source_health<T>(cache: &SourceCache<T>, now, max_age) -> serde_json::Value`); `spawn_pollers` = today's `spawn_family_pollers` body using its own fields; `load_templates` = `minijinja_embed::load_templates!(*env)`.
  - `pub struct HomelabDashboard;` implementing `Dashboard` (`view` → `"homelab.html"`, `health` → `{}`, no pollers).
  - `src/family/refresh.rs`: `refresh_calendar_once`, `refresh_weather_once`, `refresh_timetable_once`, `describe`, `fail`, `stored` (moved from `scheduler.rs`, same signatures).

- [ ] **Step 1: Write failing tests** (in `src/family/dashboard.rs`)

- `family_view_matches_direct_build`: seeded caches (same seeding as today's device_api fixture), `view(warsaw(2026,11,25,7,0))` → template `"family.html"` and context equal to `Value::from_serialize(&build(&sample.inputs(now)))`.
- `family_health_shape`: keys `calendar`, `weather`, `timetables` (array of 2), each with `fetched_at`, `stale`, `last_error`.
- `healthz_on_empty_caches_reports_stale`: fresh tempdir → every source `stale: true`, `fetched_at: null`, `last_error: null`. *(Review Focus 4)*
- `prepare_does_not_touch_the_network`: `ParsedFamily` with endpoints pointing at a wiremock server; `prepare` then drop; assert the mock received 0 requests.
- `homelab_view_is_placeholder`: template `"homelab.html"`, `message` == `"Homelab: wkrótce"`.

- [ ] **Step 2: Run** `cargo test family::dashboard homelab` — expected FAIL.

- [ ] **Step 3: Implement.** Move `scheduler.rs` tests with their functions into `family/refresh.rs`.

- [ ] **Step 4: Run** `mise run lint && mise run test` — PASS.

- [ ] **Step 5: Commit** `refactor: implement the Dashboard trait for family and homelab`

---

### Task 5: Route the device API and renderer through dashboards

**Files:**
- Modify: `src/device_api/mod.rs`, `src/device_api/refresh.rs` (tests only), `src/render/mod.rs`, `src/dashboards/test_support.rs`
- Create: `src/device_api/stub.rs` (`#[cfg(test)]`), `src/family/render_tests.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Task 3/4.
- Produces:
  - `pub type Active = BTreeMap<&'static str, Arc<dyn Dashboard>>;` in `src/dashboard.rs`.
  - `AppState { cfg: Arc<Config>, dashboards: Arc<Active>, renderer: Arc<Renderer>, images: Arc<ImageStore>, clock: Clock }`.
  - `Renderer::new(src: TemplateSource, dashboards: &Active) -> anyhow::Result<Renderer>`: `Embedded` → one `Environment` per dashboard filled by `load_templates`, and each environment's templates are loaded eagerly (render nothing, but `get_template` every name the bundle registered, failing startup on error); `Dir(path)` → today's single flat `AutoReloader`, lazy.
  - `Renderer::render(&self, dashboard: &str, view: &View, depth: BitDepth) -> anyhow::Result<Vec<u8>>`.
  - `pub fn render_dashboard(state: &AppState, name: &str, now: DateTime<Utc>, depth: BitDepth) -> anyhow::Result<Vec<u8>>` (view + rasterize, run via `spawn_blocking` as today).
  - `/healthz` → `{"status":"ok","dashboards":{name: dashboard.health(now)}}`.
  - `display` uses `dashboards[device.dashboard].switch_at()`.
  - `preview` maps `{name}.png` to `dashboards` (404 otherwise).
  - `src/device_api/stub.rs`: `pub struct StubDashboard { pub name: &'static str, pub fail_render: AtomicBool, pub spawned: AtomicUsize, pub switch_at: Option<NaiveTime> }` with a tiny inline template (`load_templates` adds `"stub.html"` via `env.add_template`).

The single-crate build still has one embedded bundle; in this task every dashboard's `load_templates` loads that same bundle (names do not collide yet). Task 9/10 give each crate its own bundle.

- [ ] **Step 1: Rewrite core tests against the stub, move content tests to family**

In `device_api` tests the fixture uses two stubs (`"family"`-free names, e.g. `"alpha"` with `switch_at = Some(18:00)` and `"bravo"`); keep every existing test's assertions where they are protocol-level (setup known/unknown MAC, bad token, numeric refresh rate + stable filename, PNG at image_url, fallback to previous filename on render failure via `fail_render`, preview `at`/`depth`, healthz staleness → now healthz shape with stub JSON). New:
- `setup_image_url_uses_dashboard_name_and_depth`: device on `"alpha"` depth 1 → `image_url` == `"{BASE}/preview/alpha.png?depth=1"`. *(Review Focus 2)*
- `preview_query_parsing_applies_to_any_dashboard`: `/preview/bravo.png?at=2026-11-25T07:00:00 01:00` → 200; `?at=nope` → 400; `?depth=3` → 400; `/preview/zulu.png` → 404. *(Review Focus 5)*
- `healthz_lists_each_active_dashboard`: `{"status":"ok","dashboards":{"alpha":…,"bravo":…}}`.
- `display_uses_dashboard_switch_at`: at 17:50 Warsaw, device on `"alpha"` → `refresh_rate` 660 (18:00 + 60 s); on `"bravo"` → 900.

`device_api/refresh.rs` tests: replace `test_support` imports with local `warsaw`/`time` helpers so the module no longer imports family code.

Move to `src/family/render_tests.rs` (renderer built with family + homelab active): `family_render_is_valid_png` (both depths, ≤ `MAX_PNG_BYTES`), `worst_case_family_render_fits_size_limit`, `homelab_render_is_valid_png`, `embedded_and_dir_templates_match`, `write_previews` (still writes to `<workspace root>/target/preview/`), `dir_templates_reload_after_edit`, plus new:
- `dir_reload_after_editing_included_svg`: copy `templates/` to a tempdir, render family with an icon, edit `icons/<that>.svg`, render again → bytes differ.
- `family_only_override_dir`: tempdir with only `family.html` + `icons/`; through the router: `/preview/family.png` 200, `/preview/homelab.png` 500; a homelab device's `/api/display` falls back to its previous image (seed one first) and returns 503 when it has none.
- `preview_family_at_switch` (was `preview_honors_at_and_depth`'s family-specific part): family preview at 07:00 vs 19:00 differ.

- [ ] **Step 2: Run** `cargo test` — expected FAIL until implemented.

- [ ] **Step 3: Implement** the interfaces; delete `dashboard_name`/`DashboardKind` leftovers and the Task 3 interim wiring in `device_api`.

- [ ] **Step 4: Verify the core/family boundary**

Run: `grep -rnE "family|homelab|crate::(sources|text|dashboards)" src/device_api src/render src/polling src/cache.rs src/config.rs src/dashboard.rs | grep -v "cfg(test)"`
Expected: matches only inside `#[cfg(test)]` modules (inspect each).

- [ ] **Step 5: Run** `mise run lint && mise run test` — PASS.

- [ ] **Step 6: Commit** `refactor: route the device API and renderer through registered dashboards`

---

### Task 6: Startup order, server module, typed capture

**Files:**
- Create: `src/server.rs`
- Modify: `src/main.rs`, `src/sources/librus/capture.rs`

**Interfaces:**
- Produces (`src/server.rs`):
  ```rust
  pub struct ServeOptions { pub templates: TemplateSource, pub stop_timeout: Duration }
  pub async fn run(
      loaded: Loaded,
      opts: ServeOptions,
      shutdown: impl Future<Output = ()> + Send + 'static,
  ) -> anyhow::Result<()>
  ```
  Order inside `run`: `prepare` every parsed entry (`PrepareContext { timezone, data_dir }`) → `Renderer::new` → bind `cfg.listen` → `spawn_pollers` for each → `axum::serve` with graceful shutdown → cancel → wait ≤ `stop_timeout` (1 s in production). `main` installs signal handlers (today's `shutdown_signal`) before calling `run`.
- Produces (`capture.rs`): `pub async fn capture(kids: &[KidConfig], out: &Path) -> anyhow::Result<()>` and `capture_with_urls(kids, out, urls)`; still `ensure!(!kids.is_empty(), "no [[family.kid]] entries in the config")`.
- `main` for capture: `config::load(...)` with the full entry list, take the `family` `ParsedEntry`, upcast `Box<dyn ParsedDashboard>` to `Box<dyn Any>`, `downcast::<ParsedFamily>()`, call `capture(&parsed.cfg.kids, &out)`. No `prepare`.
- `main` entry list: `fn entries() -> Vec<DashboardEntry> { vec![family::entry(), dashboards::homelab::entry()] }`.

- [ ] **Step 1: Write failing tests** (in `src/server.rs`, using a counting stub `ParsedDashboard`/`Dashboard` whose `spawn_pollers` increments a shared `AtomicUsize` and spawns one task that awaits cancellation)

- `occupied_port_starts_no_pollers`: bind a `TcpListener` on `127.0.0.1:0` first, config `listen` = that address; `run` → `Err`, spawned count 0.
- `failing_prepare_starts_no_pollers`: stub `prepare` returns `Err` → `run` → `Err`, spawned count 0, port never bound (bind the same port afterwards succeeds).
- `shutdown_with_hanging_poller_returns_within_budget`: stub poller ignores cancellation (`std::future::pending()`); `run` with `stop_timeout = 1 s` and a shutdown future that resolves after the server is up → returns `Ok` within 1.5 s (`tokio::time::timeout`). *(Review Focus 3)*
- Capture (in `src/family/config.rs` or `capture.rs` tests, calling the same helper `main` uses, factored as `pub fn family_kids(loaded: Loaded) -> anyhow::Result<Vec<KidConfig>>` in `src/family/dashboard.rs`):
  - `capture_rejects_invalid_unrelated_device` (bad MAC) → load `Err`.
  - `capture_accepts_zero_devices_with_kids` → kids len 1.
  - `capture_reads_each_secret_once`: `loaded.secrets.reads()` == number of secret fields in the config (no second parse).

- [ ] **Step 2: Run** `cargo test server capture` — FAIL.

- [ ] **Step 3: Implement**; `main.rs` keeps only CLI parsing, logging, signal setup, `entries()`, and the two command arms.

- [ ] **Step 4: Run** `mise run lint && mise run test` — PASS.

- [ ] **Step 5: Commit** `refactor: start pollers only after bind through a dashboard-agnostic server`

---

### Task 7: Sanitize response-derived errors

Spec: "Security of public endpoints", deliberate change 2.

**Files:**
- Modify: `src/sources/mod.rs`, `src/sources/calendar.rs`, `src/sources/weather.rs`, `src/sources/librus/{auth.rs,timetable.rs}`, `src/family/refresh.rs`

**Interfaces:**
- `SourceError::Parse(&'static str)` (was `String`), so parse messages cannot carry input text. `Auth(String)` stays but every `auth(...)` call site is audited: only step names, HTTP status, and `describe(url)` (host + path) may be formatted in. `CredentialsRejected(String)` same rule; variant unchanged.
- Fixed messages: calendar `"calendar is not valid iCalendar"` (keep `"not a VCALENDAR"`), weather `"forecast JSON does not match the expected shape"`, `"bad daily date"`, `"bad hourly time"`, `"missing daily entry"`, `"hourly arrays differ in length"`; timetable `"Librus returned a different week than requested"`.
- Structured logging only: `debug!(source = "weather", line = e.line(), column = e.column(), "parse failed")` style; never `{e}` of a parser error, never body text.

- [ ] **Step 1: Write failing tests** (in `src/family/refresh.rs`, with `tracing-subscriber`'s test writer capturing at DEBUG into a buffer)

- `malformed_ics_never_leaks_its_content`: wiremock serves `BEGIN:VCALENDAR\nSECRETSENTINEL-line\nEND:VCALENDAR`-style malformed ICS; run `refresh_calendar_once`; assert `SECRETSENTINEL` absent from the returned error (`{e}`, `{e:?}`, `anyhow!(e)` `{:#}`), from `cache.last_error()`, from `source_health(...)` JSON, and from captured logs.
- `failed_request_never_leaks_secret_url` (extends today's `errors_never_leak_the_calendar_url` with the captured-log assertion).
- `malformed_forecast_never_leaks_its_content` (weather body with sentinel in a date string).
- `credentials_rejected_still_classified`: existing auth tests still match `CredentialsRejected`; `to_outcome` test unchanged.

- [ ] **Step 2: Run** — FAIL (calendar parser text currently flows through).

- [ ] **Step 3: Implement.**

- [ ] **Step 4: Run** `mise run lint && mise run test` — PASS.

- [ ] **Step 5: Commit** `fix: keep response content out of source error messages`

---

### Task 8: Convert to a workspace (one member)

**Files:**
- Create: `Cargo.toml` (virtual workspace), `crates/tablica/Cargo.toml`
- Move: `src/` → `crates/tablica/src/`, `build.rs` → `crates/tablica/build.rs`, `templates/` → `crates/tablica/templates/`, `tests/fixtures/` → `crates/tablica/tests/fixtures/` (use `git mv`; owner `kid*` captures are untracked, move them with plain `mv`)

  Note (ruling R8): the ten existing `kid1` fixtures are already tracked, in redacted form, so they moved with `git mv` like the rest. The "never commit" rule covers new local captures, which stay untracked.
- Modify: `.dockerignore`, `.github/workflows/image.yml`, `contrib/docker/Dockerfile`, `mise.toml`, `release-please-config.json`

- [ ] **Step 1: Root `Cargo.toml`**

`[workspace] resolver = "3"`, `members = ["crates/*"]`; `[workspace.package] version = "0.1.3"` (or the then-current release), `edition = "2024"`, `rust-version = "1.100"`, `license = "MIT"`; every current dependency into `[workspace.dependencies]` with today's versions/features. `crates/tablica/Cargo.toml`: `name = "tablica"`, `version.workspace = true` etc., deps as `x.workspace = true`, same `description`.

- [ ] **Step 2: Fix paths.** `include_bytes!` of the font → `../../../../assets/fonts/Inter.ttf` (relative to `crates/tablica/src/render/`); tests that read `config.sample.toml`, `templates/…`, `tests/fixtures/…` use `env!("CARGO_MANIFEST_DIR")`-relative or workspace-root paths; `write_previews` writes to `<workspace root>/target/preview`.

- [ ] **Step 3: Docker and CI.** `.dockerignore`: replace `tests/fixtures/librus/kid*` with `**/tests/fixtures/librus/kid*`. Dockerfile: `cargo build --release --locked -p tablica`. `image.yml` path filter adds `"crates/**"`; smoke test additionally `curl -fsS -o /dev/null` both `/preview/family.png` and `/preview/homelab.png`. `mise.toml`: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check`, `cargo test --workspace`.

- [ ] **Step 4: release-please config for the virtual workspace**

`packages["."]`: `"release-type": "simple"`, `"extra-files"`:
```json
[ { "type": "toml", "path": "Cargo.toml", "jsonpath": "$.workspace.package.version" },
  { "type": "toml", "path": "Cargo.lock", "jsonpath": "$.package[?(!@.source)].version" } ]
```

- [ ] **Step 5: Verify**

- `mise run lint && mise run test` — PASS.
- `docker build -f contrib/docker/Dockerfile .` succeeds; build-context check: `docker build --no-cache -f - . <<<'FROM busybox
COPY . /ctx
RUN find /ctx -path "*fixtures/librus/kid*" | wc -l'` prints `0`.
- Owner-fixture discovery: with captures present locally, `cargo test -p tablica owner_fixture -- --nocapture` shows the fixture tests ran on `kid1` (temporarily add a log line or count; remove before commit).
- Repeat Task 1 Step 4's dry-run cases (feat under `crates/`, fix under `assets/`, refactor, perf, breaking) on scratch branches containing this commit (user OK first). On each generated release-PR head, in a clean checkout: `cargo metadata --format-version 1 --no-deps | jq -r '.packages[].version'` all equal the proposed version, `grep -A1 'name = "tablica' Cargo.lock` versions equal it, `cargo build --locked` passes, `./target/debug/tablica --version` prints it, `git diff` of `Cargo.lock` touches only workspace package versions.

- [ ] **Step 6: Commit** `build: convert to a Cargo workspace`

---

### Task 9: Extract `tablica-core`

**Files:**
- Create: `crates/core/Cargo.toml` (`name = "tablica-core"`, `publish = false`), `crates/core/src/lib.rs`
- Move: `cache.rs`, `config.rs`, `dashboard.rs`, `server.rs`, `polling/`, `render/`, `device_api/` from `crates/tablica/src/` to `crates/core/src/`
- Modify: `crates/tablica/Cargo.toml` (depend on `tablica-core`), imports in remaining modules (`crate::cache` → `tablica_core::cache`, etc.)

**Interfaces:**
- Produces: `tablica_core::{cache, config, dashboard, server, polling, render, device_api}` with the same items as Tasks 3–6. The font path becomes `../../../assets/fonts/Inter.ttf` relative to `crates/core/src/render/`.

- [ ] **Step 1: Move and fix imports** (`git mv`); core's `Cargo.toml` lists only what core uses.
- [ ] **Step 2: Verify** `cargo tree -p tablica-core | grep -E "tablica-(family|homelab)"` prints nothing; `grep -rn family crates/core/src | grep -v cfg\(test\)` shows only test code; `mise run lint && mise run test` PASS.
- [ ] **Step 3: Commit** `refactor: extract tablica-core`

---

### Task 10: Extract `tablica-family` and `tablica-homelab`

**Files:**
- Create: `crates/family/{Cargo.toml,build.rs,src/lib.rs}`, `crates/homelab/{Cargo.toml,build.rs,src/lib.rs}`
- Move: `family/`, `dashboards/family.rs` (→ `view.rs`), `dashboards/test_support.rs`, `sources/`, `text/`, `templates/family.html`, `templates/icons/`, `tests/fixtures/` → `crates/family/…`; `dashboards/homelab.rs` (→ `lib.rs`), `templates/homelab.html`, `HOMELAB_PLACEHOLDER` → `crates/homelab/…`
- Modify: `crates/tablica/Cargo.toml`, `crates/tablica/build.rs` (deleted: binary has no templates), `THIRD_PARTY_NOTICES.md` (Librus source path → `crates/family/src/sources/librus/auth.rs`)

**Interfaces:**
- Produces: `tablica_family::{entry, ParsedFamily, FamilyConfig, KidConfig, capture, family_kids}`, `tablica_homelab::entry`.
- Each dashboard crate: `minijinja` and `minijinja-embed` as normal deps, `minijinja-embed` as build dep; `build.rs` = `minijinja_embed::embed_templates!("templates");`; `load_templates` = `minijinja_embed::load_templates!(*env);`.

- [ ] **Step 1: Write the failing test** `crates/core` test `embedded_bundles_are_isolated`: two stub dashboards each `add_template("shared.html", …)` with different content; rendering `"shared.html"` for each yields different PNGs.
- [ ] **Step 2: Move family, then homelab** (`git mv`; owner captures with `mv`), fix imports and fixture/template paths.
- [ ] **Step 3: Verify** both bundles render (`family_render_is_valid_png`, `homelab_render_is_valid_png` now in their crates), `git status --ignored` shows no `kid*` capture staged, `mise run lint && mise run test` PASS.
- [ ] **Step 4: Commit** `refactor: extract tablica-family and tablica-homelab`

---

### Task 11: Slim binary and end-to-end test

**Files:**
- Modify: `crates/tablica/src/main.rs`, `crates/tablica/Cargo.toml`
- Delete: `crates/tablica/src/lib.rs` and any leftover modules
- Create: `crates/tablica/tests/end_to_end.rs`

**Interfaces:**
- `main.rs`: CLI (`serve`, `capture librus`), logging, signals, `entries() = vec![tablica_family::entry(), tablica_homelab::entry()]`, `tablica_core::server::run`, capture via `tablica_family::family_kids` + `capture`.
- To make the e2e test drive real wiring without a listener, `tablica_core::device_api::router(AppState)` and `tablica_core::server::prepare_state(loaded, templates, clock) -> anyhow::Result<(AppState, Vec<Arc<dyn Dashboard>>)>` (factor out of `run`; `run` calls it).

- [ ] **Step 1: Write the test** `end_to_end_family_and_homelab`: config = `config.sample.toml` with secret files in a tempdir; seed the family caches in `data_dir` with `tablica_family::test_support`-equivalent data (expose a `pub fn seed_sample_caches(data_dir, now)` from family behind a `test-support` feature, enabled as a dev-dependency feature by the binary); fixed clock Wednesday 2026-11-25 07:00 Warsaw. Assert: kitchen `/api/display` PNG is 2-bit, office is 1-bit (set `bit_depth = 1` for office in the test config); with the clock at 15:50 Warsaw, kitchen `refresh_rate` is 660 (sample `switch_at` 16:00 + 60 s) and office's is 900; `/preview/family.png` and `/preview/homelab.png` 200; `/healthz` has `dashboards.family.timetables` of length 2.
- [ ] **Step 2: Run** `cargo test -p tablica` — PASS after slimming.
- [ ] **Step 3: Run** `mise run lint && mise run test`, `docker build -f contrib/docker/Dockerfile .` and run the image with `config.sample.toml` + dummy secrets: `/healthz` returns `"status":"ok"`, both previews 200.
- [ ] **Step 4: Commit** `refactor: reduce the tablica crate to the binary`

---

### Task 12: Docs

**Files:**
- Modify: `README.md` (Contributing: workspace layout, "adding a dashboard" steps from the spec's Goals, `cargo test --workspace`), `docs/superpowers/specs/2026-11-26-dashboard-crates-design.md` only if something was decided differently during implementation (say so in the commit body)

- [ ] **Step 1: Write the docs.**
- [ ] **Step 2: Commit** `docs: describe the workspace and how to add a dashboard`
