# Dashboard crates: design

Revision 4: incorporates three design reviews (Codex, GPT-6.2-Astra). Revision
3 replaced release-plz with release-please (decided with the owner).

## Why

A second dashboard (homelab) is coming, with data aggregated from several APIs
(Synology, UniFi; details still open). Today the family dashboard is woven
through the TRMNL server: `device_api` matches on `DashboardKind`, `AppState`
holds `FamilyCaches`, `Config` has a mandatory `family` field, `/healthz` lists
family sources by name, and `display` reads `cfg.family.switch_at`. Adding
homelab on top of that would mean a second set of special cases in every one of
those places.

The goal is to split the project into a Cargo workspace where the TRMNL side
knows nothing about any particular dashboard, and each dashboard is its own
crate that plugs in by implementing a trait.

## Goals

- A `core` crate that owns everything device-facing: the BYOS device API,
  preview, health endpoint, rendering HTML to PNG, refresh timing, device and
  global config parsing, secret resolution, the generic poller, and the source
  cache.
- One crate per dashboard (`family`, `homelab`) that owns its config section,
  sources (including their HTTP clients and sessions), caches, pollers, health
  report, view model, on-screen text and templates.
- A thin binary crate that lists the dashboards and runs the server.
- Adding a dashboard = a new crate under `crates/` (picked up by the member
  glob), a Cargo dependency in the binary, and one line in the binary's
  dashboard list. No edits to `core`, no release configuration.

## Non-goals

- Building the homelab dashboard. It stays the current placeholder, moved into
  its own crate.
- Runtime plugins (dynamic loading, separate processes). Dashboards are
  compiled in.
- Publishing crates to crates.io, or keeping a Rust library API for path/git
  consumers. Today's `src/lib.rs` exports exist for the binary and tests only;
  after the split the supported surface is the `tablica` binary, its config
  file, and its HTTP endpoints.
- A shared "sources" crate. Sources live in the dashboard that uses them; one
  moves to a shared crate only when a second dashboard needs it.
- A generic session/login abstraction in core. Deferred to the homelab design,
  together with: session renewal, bounding repeated logins after a credential
  rejection (`IntervalPolicy` today fetches immediately at startup and retries a
  rejection at the regular period), keeping each source's last good data during
  a partial outage, and deciding which homelab data is fit for the public
  endpoints.

## Anti-goals (a failure even if it compiles)

- Any user-visible behaviour change beyond those listed under "Deliberate
  changes". Same images, same image filenames, same refresh rates, same
  previews, same template override behaviour, same config validation.
- An existing `config.toml`, `/data` directory, or `--templates` directory that
  stops working.
- Losing cached data across the upgrade. An unreadable cache becomes empty
  (`cache.rs` ignores incompatible JSON), and an empty timetable cache triggers
  Librus logins at startup.
- Any path to Librus logins before the server has bound its port (a bind
  failure under a restart loop must never log in).
- `core` depending on `family` or `homelab`, or containing the word "family"
  anywhere outside tests.
- A release pipeline that drops a `feat`/`fix` commit anywhere in the repo from
  the changelog, or fails to open a release PR for it.
- Owner Librus captures reaching the Docker build context or its cache layers.

## Approaches considered

**A. `Dashboard` trait objects, constructed by a list in the binary (chosen).**
Core defines the contract; the binary holds a list of entries. Core routes by
name and never knows family exists. The view context crosses the boundary
type-erased (`minijinja::Value`); nothing is lost since templates are untyped
anyway, and each crate keeps its typed view model internally. (Revision 1's
`DashboardFactory` trait was dropped: plain functions are enough.)

**B. Core generic over a closed enum defined in the binary.** Static dispatch,
but every new dashboard means editing the enum and its delegating `match` arms.
Rejected.

**Rendering boundary.** Dashboards could return finished HTML, giving a thinner
trait, but then every dashboard re-implements MiniJinja setup and hot reload.
Chosen instead: dashboards ship templates, core renders them.

**Data model.** Homelab data will come from login-session APIs with several
calls per refresh. Fetching at `/api/display` time would tie screen latency and
availability to those APIs. Chosen: background pollers into a cache, renders
read only the cache. So `view()` is synchronous.

**Release tooling.** release-plz works per package: it attributes commits by
package directory and looks up per-package history under tags. After the
split, library crates have no history under the existing `v0.1.3` tag, the
root `assets/` font belongs to no package, and every new dashboard would need
its own release configuration. Patching around that (shared version group,
custom tag patterns, `changelog_include`, moving the font) fights the tool.
Chosen: release-please with one repo-wide component, which scans every
conventional commit since the last `v` tag. release-plz is the more common tool
in Rust, but it is built for crates published separately; tablica is one
application with one version and one image.

## Workspace layout

```
Cargo.toml                 # virtual [workspace], resolver = "3", members = ["crates/*"],
                           # [workspace.package] + [workspace.dependencies]
crates/
  core/      tablica-core     lib
  family/    tablica-family   lib   (build.rs embeds its templates/)
  homelab/   tablica-homelab  lib   (build.rs embeds its templates/)
  tablica/   tablica          bin   (main.rs, CLI)
assets/fonts/              # stays at the root; core include_bytes! it
docs/, contrib/, config.sample.toml, CHANGELOG.md  # stay at the root
```

Module moves (current → new):

| Current | Goes to |
|---|---|
| `cache.rs`, `polling/`, `render/`, `device_api/` | core |
| `config.rs`: global keys, schedule, devices, secret resolution, `normalize_mac`, `BitDepth`, `ConfigError` | core |
| `config.rs`: `FamilyConfig`, `KidConfig`, `WeatherConfig`, raw family shapes, their redacting `Debug` impls and tests | family |
| `family/`, `dashboards/family.rs`, `dashboards/test_support.rs`, `sources/` (incl. `SourceError`, Librus capture), `text/` (`Condition`, `PrecipKind`, `pl.rs`), `scheduler.rs` (refresh functions, `describe`/`fail` sanitizing) | family |
| `templates/family.html`, `templates/icons/` | `crates/family/templates/` |
| `dashboards/homelab.rs`, `templates/homelab.html`, `HOMELAB_PLACEHOLDER` | homelab |
| `main.rs` | tablica |
| `tests/fixtures/{ics,weather,librus}` | `crates/family/tests/fixtures/` |

`DashboardKind` is removed; a device's `dashboard` becomes a validated `String`.

**Fixtures and Docker.** `.dockerignore` currently excludes
`tests/fixtures/librus/kid*`. The exclusion is generalized to
`**/tests/fixtures/librus/kid*` in the same commit as the first fixture move;
the handmade fixture stays included. Verify with a build-context listing that
no `kid*` capture is present. The owner-fixture tests in `timetable.rs` pass
silently when no captures exist, so verify once after the move that they still
discover the local captures.

## The plug-in contract (in `core`)

Two phases, so that validation, offline construction and starting work are
separate. Signatures are indicative; the builder may adjust names and
lifetimes.

```rust
/// One entry per dashboard crate, listed in the binary.
pub struct DashboardEntry {
    /// Canonical key: config section, device `dashboard = "..."`, preview stem,
    /// image filename prefix.
    pub name: &'static str,
    /// Phase 1: parse and validate the section (`None` = absent from the file),
    /// reading secrets. No caches opened, no clients, no network.
    /// `Ok(None)` = this dashboard is not active.
    pub parse: fn(section: Option<&toml::Value>, secrets: &Secrets) -> Result<Option<Box<dyn ParsedDashboard>>, ConfigError>,
}

/// `Any` so the binary can retrieve a dashboard's typed parsed config
/// (capture needs family's kids) without parsing or reading secrets twice.
pub trait ParsedDashboard: Any + Send {
    /// Phase 2 (serve only, before bind): open caches, build HTTP clients and
    /// anything else fallible. Offline: no network.
    fn prepare(self: Box<Self>, ctx: &PrepareContext) -> anyhow::Result<Arc<dyn Dashboard>>;
}

pub struct PrepareContext<'a> { pub timezone: Tz, pub data_dir: &'a Path }

pub trait Dashboard: Send + Sync {
    /// Registers the embedded templates into this dashboard's own environment.
    fn load_templates(&self, env: &mut minijinja::Environment<'static>);
    /// Phase 3 (after bind): start background pollers. Infallible.
    fn spawn_pollers(self: Arc<Self>, shutdown: CancellationToken) -> Vec<JoinHandle<()>>;
    /// Template name and context for a render at `now`. Reads caches only. CPU-bound.
    fn view(&self, now: DateTime<Tz>) -> anyhow::Result<View>;
    /// A local time of day the device should wake just after (family: `switch_at`).
    fn switch_at(&self) -> Option<NaiveTime> { None }
    /// JSON for `/healthz`. Only sanitized text (see Security).
    fn health(&self, now: DateTime<Utc>) -> serde_json::Value;
}

pub struct View { pub template: &'static str, pub context: minijinja::Value }
```

The live instance has no `name()`; the entry's `name` is the single canonical
key and core keeps the mapping.

`ConfigError` (core-owned) gains a generic variant, e.g.
`Dashboard { section: String, message: String }`, so a new dashboard can report
semantic validation failures without editing core. Secret errors keep today's
owner-qualified variants via `Secrets`.

Core also exports what dashboards need: `SourceCache`, `Stamped`, `is_stale`,
the `polling` module, `Secrets`/`SecretRef` (today's rules: the file wins, only
a missing file falls back to env, values trimmed, owner-qualified errors), and
`source_health(cache, now, max_age)` producing today's per-source JSON.

**HTTP.** Core does not supply an HTTP client. Family builds its default
client (10 s connect / 30 s total, as today) in `prepare`, so a failure aborts
startup as it does now. Librus keeps its per-account cookie jar with redirects
disabled, created per refresh. Endpoints stay injectable for tests inside the
family crate, as `Sources` does today.

### Names

Validated at startup: `[a-z]{5,7}`, unique across entries, and not one of
core's top-level config keys (`timezone`, `base_url`, `listen`, `data_dir`,
`schedule`, `device`). Why 5–7: image filenames are
`{dashboard}-{6 hex of sha256(device)}-{unix secs}` (`device_api/images.rs`),
and the firmware treats the first 13 characters as the screen's identity. With
5–7 letters those 13 characters never include timestamp digits, and the whole
name stays within the 31-character budget. At 7 letters only 5 of the 6
device-hash digits are in the prefix (true for `homelab` today), so two devices
on a 7-letter dashboard collide with probability about 1 in a million; accepted,
and documented in `images.rs`. `family` and `homelab` filenames stay
byte-for-byte the same.

Tests: first-13 stability for a 5-letter name across widely separated
timestamps; a 7-letter name within budget; two devices on the same dashboard
get distinct prefixes.

### Which dashboards are active

Today `[family]` is required and family always polls and previews; homelab
needs no config and always previews. That is kept:

- Family's `parse` returns an error when `[family]` is absent or malformed,
  independent of the number of devices. Homelab's `parse` always returns
  `Some` and ignores any `[homelab]` settings (unknown top-level sections are
  ignored today).
- Every active dashboard polls and can be previewed, whether or not a device
  uses it. Zero devices stays valid.
- Registered-name validation (above) is separate from per-device references:
  a device naming a registered but inactive dashboard, or an unregistered one,
  is a config error.
- Whether a future dashboard's section is optional is decided in its own
  `parse`.

Tests: missing family with zero devices (error), malformed family with zero
devices (error), both previews with zero devices, a device referencing an
inactive stub, an unknown top-level section ignored.

### Startup order

1. Load config: globals, devices, then each entry's `parse`. Reads the config
   and secret files only.
2. `prepare` every parsed dashboard (caches opened once per dashboard,
   clients built). A failure exits here.
3. Build the renderer and check every embedded bundle loads.
4. Install signal handlers. (Today they are installed after spawning; moving
   them earlier is a deliberate, invisible improvement.)
5. Bind the listener. A bind failure exits here.
6. Only now call `spawn_pollers` on each active dashboard.
7. Serve; on shutdown cancel pollers, keeping the 1 s total stop budget and
   cancellation of in-flight fetches.

Tests (with a counting stub dashboard): a failing `prepare` and an occupied
port each exit with zero pollers started.

### Rendering and routing

- `AppState` holds the active dashboards by name instead of `FamilyCaches`.
- `render_dashboard` looks up the dashboard, and on the blocking pool calls
  `view()` then rasterizes (both CPU-bound, as today).
- `display` passes `dashboard.switch_at()` to `refresh_rate`.
- `preview` maps `<name>.png` to an active dashboard; anything else is 404.

### Templates

- **Embedded:** one MiniJinja environment per dashboard, loaded from that
  crate's bundle. Each crate's `build.rs` calls
  `minijinja_embed::embed_templates!("templates")`; `load_templates` calls
  `minijinja_embed::load_templates!(*env)` (reborrow; the macro expands to
  `load_template(&mut $env)`). Each dashboard crate depends on `minijinja`
  directly and on `minijinja-embed` as both a normal and a build dependency.
  The bundle lives in that crate's `OUT_DIR`, so names cannot clash across
  dashboards (`add_template` silently replaces, which a shared environment
  could not detect). Bundles are loaded at startup; the generated code
  `expect`s valid syntax, so a broken bundle fails startup and tests.
- **Override (`--templates <dir>`):** unchanged from today. One shared flat
  directory (`<dir>/family.html`, `<dir>/homelab.html`, `<dir>/icons/`) with
  auto-reload, used for every dashboard instead of the embedded environments,
  and looked up lazily at render time. A directory holding only family's files
  keeps the family display working while the homelab preview fails, as today.
  Failure responses stay as today: `/api/display` falls back to the previous
  image (503 before the first success); `/preview` returns 500. No startup
  validation of override files.

Tests: render both embedded bundles; embedded-vs-directory pixel parity; reload
after editing an included SVG as well as a top-level template; a family-only
override directory with homelab active asserts family preview 200, homelab
preview 500, and the display fallback for a homelab device separately.

## Config

The file format does not change; `config.sample.toml` and the compose file
stay valid. `core::config::load(path, secrets_dir, entries)` returns globals,
devices and the parsed (not prepared) dashboards. It does nothing else.

`tablica capture librus` keeps today's validation and never opens caches: the
binary runs the same full `load` (validating everything, including active
references), takes family's parsed result from it and downcasts it to
`tablica_family::ParsedFamily`, and passes its kids to
`tablica_family::capture(kids, out)`. Family is parsed and its secrets read
exactly once, so capture uses exactly the validated values, as today. Tests:
capture rejects an invalid unrelated device; accepts a valid zero-device config
with at least one kid (capture still rejects zero kids); resolves each secret
once (counting resolver); prepares and starts no dashboard.

Secret-bearing family config types keep their hand-written redacting `Debug`.
The secrets test is split (devices in core, family secrets in family) and
fixed: it currently searches for the full original `webcal://` URL, but the
loader rewrites it to `https://`, so it would miss a leak of the normalized
URL. Assert on a unique secret substring.

## Security of public endpoints

`/healthz` and `/preview` are unauthenticated and compose publishes port 8080.

- Source errors are sanitized at the source boundary before they are logged,
  recorded in a cache, or returned. Family keeps today's `describe`/`fail`
  stripping request URLs from reqwest errors, and additionally replaces every
  error whose text derives from the response body with a short fixed message:
  calendar parse errors (the `icalendar` parser quotes the failing line),
  weather JSON parse errors, and Librus parse/auth errors that embed page
  content. Raw response text is never logged at any level; logs may carry
  structured, input-free detail (source, stage, HTTP status, line number).
- Error classification is preserved while messages are replaced: in particular
  `CredentialsRejected` stays its own variant, since the timetable poller
  suppresses early retries on it, and collapsing it into a generic failure
  would cause extra Librus logins.
- View and health types never hold raw sessions, cookies, tokens, or response
  bodies.
- Tests with a secret sentinel, through the full refresh path: a failed request
  to a secret URL, and a malformed ICS feed containing the sentinel. Neither
  `/healthz` JSON, the cached error, the formatted `anyhow` chain, nor captured
  logs at DEBUG may contain it.

## Caches

- Family keeps its flat file names under `data_dir`: `calendar.json`,
  `weather.json`, `timetable-kid{N}.json` (N from 1, config order). These names
  are reserved for family. New dashboards write under `data_dir/<name>/`.
- A pre-split cache fixture (synthetic data, JSON written by the current code,
  committed before the split): the original `fetched_at` and data load
  unchanged, and a fresh cached timetable takes the warm-start polling path
  rather than an immediate fetch.
- One writer per cache file. `cache.rs` writes through a fixed temporary name
  per destination, so two pollers sharing a file would race.

## Deliberate changes

1. **`/healthz` shape.** Becomes
   `{"status":"ok","dashboards":{"family":{"calendar":…,"weather":…,"timetables":[…]},"homelab":{}}}`
   instead of a flat `sources` object. The image workflow only checks
   `"status":"ok"`.
2. **Error text.** Response-derived parse errors in `/healthz` `last_error`
   and the logs become short fixed messages (see Security).
3. **Release tooling and changelog style.** release-please replaces
   release-plz; new `CHANGELOG.md` sections use release-please's format (the
   section names are mapped to today's Added / Fixed / Performance). The empty
   `## [Unreleased]` heading is removed at cutover (release-please inserts new
   entries before the first `##` heading).
4. **A `perf`-only history is releasable** (today it needs a `feat` or `fix`).
   Accepted rather than worked around.

## Build and CI

- Root `Cargo.toml` becomes a virtual workspace, `resolver = "3"`,
  `members = ["crates/*"]`, with `[workspace.package]` (edition, rust-version,
  license) and `[workspace.dependencies]`. One `Cargo.lock`. Library crates are
  `publish = false`.
- `mise` tasks: `cargo clippy --workspace --all-targets -- -D warnings && cargo
  fmt --all --check`, and `cargo test --workspace`.
- Dockerfile: `cargo build --release --locked -p tablica`; binary path
  unchanged.
- `image.yml`: the path filter gains `crates/**` (keeping `assets/**`,
  `config.sample.toml`, `Cargo.*`, Docker and toolchain paths) in the first
  commit that moves code; the smoke test also fetches `/preview/family.png` and
  `/preview/homelab.png`.
- `THIRD_PARTY_NOTICES.md`: update the Librus source path. README: replace
  release-plz references.

## Release (release-please)

Requirements:

- One release unit for the whole repository: every conventional commit since
  the last `v` tag counts, wherever it lives (any crate, `assets/`, templates).
  `feat`/`fix`/`perf` open or update a release PR; `refactor`, `chore`,
  `docs`, `test`, `ci`, `build`, `style` do not open one, unless marked
  breaking (`!` or a `BREAKING CHANGE` footer).
- Bumps keep today's pre-1.0 behaviour: `feat` and `fix` are patch bumps (the
  history has feats in patch releases), breaking changes are minor bumps
  (`bump-patch-for-minor-pre-major`, `bump-minor-pre-major`). A `Release-As:`
  footer overrides.
- Tags stay `vX.Y.Z` (no component prefix); history continues from `v0.1.3`;
  the root `CHANGELOG.md` keeps its existing entries.
- Merging the release PR creates the tag and the GitHub release, pushed with
  the PAT so the tag triggers `image.yml` (tags pushed with `GITHUB_TOKEN` do
  not trigger workflows).
- The release PR head is correct on its own, in a clean checkout: every
  workspace package has the new version in Cargo metadata and in `Cargo.lock`,
  `tablica --version` prints it, no external dependency changed, and
  `cargo build --locked` passes, all before any unlocked Cargo command runs.

Shape (pin the action to an exact version, e.g. `release-please-action@v5.0.0`
with its locked `release-please@17.6.0`, and confirm details against it):

- Manifest mode (no action-level `release-type`). `release-please-config.json`
  has one package at `"."` with `include-component-in-tag: false`,
  `changelog-sections` mapping feat → Added, fix → Fixed, perf → Performance
  and hiding the rest, and the bump flags above.
- **Strategy differs before and after the split.** Step 0 (single crate) may
  use the native `rust` type. The virtual workspace must not: the native Rust
  updater does not expand `crates/*` and always updates the root
  `[package].version`, which a virtual workspace lacks. After the split use the
  `simple` type with TOML `extra-files`: `Cargo.toml` at
  `$.workspace.package.version` (every crate inherits it), and `Cargo.lock`
  at `$.package[?(!@.source)].version` (every source-less, i.e. workspace,
  package; verified against release-please 17.6.0's updater: it changes exactly
  the workspace versions, while a name-equality filter matches nothing), so
  the release commit updates all of them without a hand-kept crate list. If the filter is unsupported, fall back to a workflow step that
  regenerates `Cargo.lock` from the current release-PR head on every PR
  refresh, pushes only when it changed, and lets required checks run on the
  resulting commit. The TOML updater only warns on an unmatched path, which is
  why the clean-checkout check above is required.
- `.release-please-manifest.json` starts at `{".": "0.1.3"}`. Discovery matches
  the annotated tag `v0.1.3` (peeled commit `4aa67e3`); the first release PR
  must contain nothing at or before that commit. A `bootstrap-sha` is added
  only if discovery fails.
- `.github/workflows/release-plz.yml` is replaced by a release-please workflow
  running after CI on pushes to `main`. The existing PAT is passed through the
  action's `with: token:` input (the old workflow exported it as
  `GITHUB_TOKEN`, which the new action ignores; the default `github.token`
  would suppress downstream workflows). The secret keeps its name
  `RELEASE_PLZ_TOKEN` (a secret's value cannot be read back, so renaming means
  re-issuing the PAT); a workflow comment explains the historical name. Its
  fine-grained permissions today are Contents and Pull requests read/write.
  release-please also labels its PRs (`autorelease: pending` / `tagged`),
  which may additionally need Issues read/write; the rehearsal checks the label
  operations under the PAT, and the fix on failure is adding that permission.
- The image workflow is triggered by a tag push today, while release-please
  creates the tag through the create-release API. Verify that this fires the
  `push: tags` trigger under the PAT; if not, switch `image.yml` to
  `release: published` as its only release trigger and adjust its job guard
  and version extraction.
- `release-plz.toml` is deleted; its recovery notes are rewritten for
  release-please in the README or the workflow.

Proof, as an early gate (migration step 0, before any code moves), and
repeated at the virtual-workspace transition (step 4) for the `simple` +
extra-files shape:

- *Release PR content* (offline: release-please CLI dry-runs, fixtures, or a
  mock GitHub API; independent clean histories per case):
  1. a `feat` touching only a file under `crates/` → patch bump, one entry;
  2. a `fix` touching only `assets/` → patch bump;
  3. `refactor`-only → no release PR;
  4. `perf`-only → patch release PR; `perf` + `fix` → one release PR;
  5. a breaking `refactor!` → minor bump;
  6. the clean-checkout version check above on the generated head.
- *Merge, tag, release and events* cannot be shown by a dry run (tags are
  repository-wide, so never rehearse against the production repository by
  picking a scratch branch). Rehearse in a disposable repository the owner
  authorizes at that time; failing that, record it as pending and watch the
  first real release. Check: exactly one `vX.Y.Z` tag and one GitHub release,
  an immediate rerun creates nothing, a second release works, labels and
  follow-up CI run under the PAT, and the image workflow runs exactly once on
  the tagged commit.

If a case fails and no configuration fixes it, stop and bring it back for a
decision.

## Migration approach

A pure refactor, as a series of commits that each build and pass tests. The
seam comes first, so there is never a dependency cycle.

0. Release switch: replace release-plz with release-please on the current
   single crate (version in the root `Cargo.toml`), after the proof above. This
   is independent of the split and can ship on its own.
1. Add the pre-split cache fixture (against today's code).
2. Inside the existing crate: introduce `DashboardEntry` / `ParsedDashboard` /
   `Dashboard`, implement them for family and homelab, switch `device_api`,
   config, capture and startup over, add the counting-stub startup tests and
   the per-dashboard embedded environments. `DashboardKind`, `FamilyCaches` in
   `AppState`, and `scheduler::spawn_all` go away. Move family-dependent tests
   out of `render/` and `device_api/` (incl. `refresh.rs`) into family modules
   so core modules no longer import family.
3. Sanitize response-derived errors (with the sentinel tests).
4. Convert to a workspace with everything still in one member crate, adjusting
   `build.rs` template paths, the `.dockerignore` fixture rule, and the
   `image.yml` path filter in the same commit; version moves to
   `[workspace.package]` and the release-please config switches to the
   `simple` + extra-files shape, with the release-PR content proof repeated.
5. Extract core (now a leaf).
6. Extract family (with fixtures), then homelab.
7. Slim the binary crate.
8. README, notices, mise tasks if not already done.

**Tests.**
- Core protocol tests (`device_api`, renderer mechanics) use a small stub
  dashboard (a `#[cfg(test)]` module or a `test-support` feature, builder's
  call).
- Content-specific tests move to their owning crate: worst-case PNG size at
  both depths, embedded/directory parity and reload, render-failure fallback,
  time-dependent family output.
- An end-to-end test in the binary crate wires the real dashboards with a fixed
  clock and seeded caches: both image depths, the `switch_at` refresh, and
  both previews.
- The root-level preview output the render tests write today keeps working;
  fix relative manifest, fixture and sample-config paths.

## Left to the builder

- Exact trait method names, lifetimes, and whether `View.template` is
  `&'static str` or `String`.
- Whether `Secrets` is a struct or a function plus `SecretRef`.
- How the stub dashboard for core's tests is shared.
- Commit granularity within the migration order.
- The exact release-please configuration, within the requirements and proof
  above.

## Calls made without asking (check these)

- Crate names `tablica-core` / `-family` / `-homelab` under `crates/`.
- Config format and validation unchanged; `[family]` stays top-level and
  required.
- Cache files keep their current names; new dashboards get `data_dir/<name>/`.
- Template override directory stays flat and lazy; embedded templates are
  per dashboard.
- Dashboard names limited to `[a-z]{5,7}`.
- `/healthz` shape and sanitized error text.
- No Rust library API promised after the split.
- `capture librus` stays in the binary CLI.
- The release switch ships first, as its own step.
- Pre-1.0 bump rules kept (feat → patch); `perf`-only releases accepted.
- The release rehearsal of tags/events needs a disposable repository you
  authorize when the time comes, or is checked on the first real release.

## Implementation notes

- `config_tests`, `render_tests` and `capture_tests` live as `#[cfg(test)]` modules in
  `crates/tablica/src/lib.rs`.
- The family test helpers are behind a `test-support` feature of `tablica-family`.
- Errors for malformed dashboard sections name the field but lose line and column.
