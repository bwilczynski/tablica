# Timetable-aware Polling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace fixed Librus slots with jittered, timetable-derived polling, staggered per kid, behind a generic polling abstraction that family code plugs into.

**Architecture:** A generic `polling` module (policy trait, runner loop, jitter, backoff, `IntervalPolicy`) with no family imports. Family code (`src/family/`) owns `FamilyCaches`, the timetable-derived day windows, `TimetablePolicy`, and the function that spawns family pollers. `scheduler.rs` keeps the `refresh_*_once` functions and `Sources`, and delegates spawning to family.

**Tech Stack:** Rust 2024, tokio, chrono/chrono-tz, `rand` 0.10.

**Spec:** `docs/superpowers/specs/2026-11-26-timetable-aware-polling-design.md`

## Global Constraints

- `src/polling/**` and `src/cache.rs` must not `use crate::family`, `crate::sources`, or `crate::dashboards`.
- Randomness only through an injected `rand::rngs::StdRng`; tests use `StdRng::seed_from_u64`; production uses `rand::make_rng::<StdRng>()`. rand 0.10 names: `rand::{Rng, RngExt, SeedableRng}`, `rng.random_range(a..b)`.
- All window maths in local time (`DateTime<Tz>`); build local instants with `tz.from_local_datetime(..).earliest()`, skipping a window whose start does not exist.
- Window table (base intervals): morning rush F−2h…F 45 min; school F…L 210 min; afternoon L…18:00 150 min; day-off daytime 09:00…18:00 300 min; evening 18:00 (or L if later)…22:00 80 min if tomorrow is a school day, else 240 min; night 22:00…next window: no runs.
- In-window jitter: base × uniform(0.75, 1.25). Window-start offset: uniform 0…15 min.
- Unknown day (no cache, or date outside cached `weeks`): Mon–Fri school day with F = 08:00, L = 15:00; Sat/Sun day off.
- Librus retries: `backoff_delay(retries)` × uniform(0.75, 1.25), max 2 per scheduled run, never at or past the next scheduled run; `Rejected` never retries.
- Kid gap: a planned run within 4 min of another kid's planned run is pushed out by uniform 4…8 min.
- `IntervalPolicy`: calendar 15 min, weather 30 min, ±20 %; first run now + uniform 0…2 min; failures wait `min(backoff_delay(failures), period)` jittered.
- No new TOML config.
- Commits: conventional style, no agent attribution trailers.

## Review Focus

1. **Lessons ending after 18:00** — afternoon window empty, evening starts at L; no panic, no overlapping windows. (Task 3 test `late_lessons_skip_afternoon`.)
2. **Restart with a populated cache in the middle of the night** — first Librus run is the next morning window, not now. (Task 4 test `restart_at_night_waits_for_morning`.)
3. **DST spring-forward day** — windows still produced, no run lands in the night, no panic on non-existent local times. (Task 3 test `dst_spring_forward_day_has_runs`.)
4. **Timetable that covers only the current week, asked about the next Monday** — falls back to the unknown-day rule rather than "day off". (Task 3 test `outside_cached_weeks_is_unknown`.)
5. **Run took long enough that `now` is past the planned run** — runner sleeps zero, not panics or underflows. (Task 1 test `past_target_runs_immediately`.)

---

### Task 1: Generic polling module

**Files:**
- Modify: `Cargo.toml` (add `rand = "0.10"`; add `"test-util"` to tokio in `[dev-dependencies]` as `tokio = { version = "1.54.1", features = ["test-util", "macros", "rt"] }`)
- Create: `src/polling/mod.rs`, `src/polling/jitter.rs`, `src/polling/backoff.rs`, `src/polling/interval.rs`
- Modify: `src/lib.rs` (add `pub mod polling;`), `src/scheduler.rs` (remove `backoff_delay` + its constants + test `backoff_caps_at_60_min`; import from `crate::polling`)

**Interfaces:**
- Produces:
  - `pub enum Outcome { Ok, Failed, Rejected }` (derive `Debug, Clone, Copy, PartialEq, Eq`)
  - `pub trait PollPolicy: Send { fn first_run(&mut self, now: DateTime<Tz>) -> DateTime<Tz>; fn next_run(&mut self, now: DateTime<Tz>, outcome: Outcome) -> DateTime<Tz>; }`
  - `pub async fn run_poller<P: PollPolicy, F, Fut>(name: &'static str, tz: Tz, policy: P, shutdown: CancellationToken, job: F) where F: FnMut() -> Fut, Fut: Future<Output = Outcome>` — logs `info!("{name}: next run at {t}")` (local time) after computing each target
  - `polling::jitter::{jittered(base: Duration, spread: f64, rng: &mut StdRng) -> Duration, between(lo: Duration, hi: Duration, rng: &mut StdRng) -> Duration}` (`std::time::Duration`; `jittered` = base × uniform(1−spread, 1+spread); `between` inclusive-exclusive, returns `lo` when `lo >= hi`)
  - `polling::backoff::backoff_delay(attempt: u32) -> Duration` (moved verbatim)
  - `pub struct IntervalPolicy` with `pub fn new(period: Duration, spread: f64, rng: StdRng) -> Self`
  - re-export all of the above from `polling`

- [ ] **Step 1: Write failing tests**

In `jitter.rs`: `jittered_stays_in_bounds` (1000 draws of `jittered(60 min, 0.25)` all in [45, 75] min, and not all equal); `between_degenerate_returns_lo`.
In `backoff.rs`: move `backoff_caps_at_60_min` as-is.
In `interval.rs`: `first_run_within_two_minutes` (in [now, now+2 min)); `ok_waits_period_jittered` (15 min ±20 % → [12, 18] min); `failures_back_off_capped_at_period` (period 30 min: failure 1 → [4, 6] min, failure 2 → [8, 12], failure 3 → [16, 24], failure 4 → ≤ 36 min i.e. min(40,30)×1.2; then `Ok` resets so the next `Failed` is back to [4, 6]).
In `mod.rs` (`#[tokio::test(start_paused = true)]`): `runs_on_policy_schedule_until_shutdown` (scripted policy: first_run = now + 1 min, next_run = now + 10 min; advance 25 min → job ran 3 times; cancel → task completes); `past_target_runs_immediately` (policy returns `now - 5 min` → job runs again without time advancing, bounded by a counter that cancels after 3 runs).

- [ ] **Step 2: Run and verify they fail**

Run: `cargo test polling::`
Expected: compile errors (module missing).

- [ ] **Step 3: Implement the module**

`run_poller`: compute target from the policy with `Utc::now().with_timezone(&tz)`; sleep `(target - now).to_std().unwrap_or_default()` inside `tokio::select!` with `shutdown.cancelled()`; run the job inside the same kind of `select!`; loop. Note: under paused tokio time `Utc::now()` does not advance — use `tokio::time::Instant` for sleeping: convert the chrono difference to a `Duration` and `sleep` that, so the tests drive it by `tokio::time::advance`. In tests the scripted policy ignores `now`'s absolute value and only the returned difference matters.

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: all pass (existing scheduler tests still compile against the moved `backoff_delay`).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/polling src/scheduler.rs
git commit -m "feat(polling): add generic poll policy, runner and jitter"
```

### Task 2: Move family caches into a family module

**Files:**
- Create: `src/family/mod.rs`
- Modify: `src/cache.rs` (remove `Caches`, `*_MAX_AGE`, family imports), `src/lib.rs` (`pub mod family;`), `src/main.rs`, `src/device_api/mod.rs`, `src/scheduler.rs`

**Interfaces:**
- Produces: `family::FamilyCaches { pub calendar, pub weather, pub timetables }` with `pub fn open(data_dir: &Path, kids: usize) -> FamilyCaches` (body moved verbatim); `family::{CALENDAR_MAX_AGE, TIMETABLE_MAX_AGE, WEATHER_MAX_AGE}`.
- `cache.rs` keeps `Stamped`, `SourceCache`, `is_stale` only.

- [ ] **Step 1: Move the code and rename `Caches` → `FamilyCaches` at every use site** (pure move; any `Caches` tests in `cache.rs` move with it).

- [ ] **Step 2: Verify the boundary and tests**

Run: `grep -nE "crate::(family|sources|dashboards)" src/cache.rs src/polling -r; cargo test`
Expected: grep prints nothing; all tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/family src/cache.rs src/lib.rs src/main.rs src/device_api/mod.rs src/scheduler.rs
git commit -m "refactor: move family caches out of the generic cache module"
```

### Task 3: Day windows from the timetable

**Files:**
- Create: `src/family/polling/mod.rs`, `src/family/polling/windows.rs`
- Modify: `src/family/mod.rs` (`pub mod polling;`)

**Interfaces:**
- Consumes: `sources::librus::timetable::Timetable` (`weeks: Vec<NaiveDate>` Mondays, `days: BTreeMap<NaiveDate, Vec<Lesson>>`), `polling::jitter`.
- Produces:
  - `pub enum Day { School { first: NaiveTime, last: NaiveTime }, Off }`
  - `pub fn day_for(date: NaiveDate, tt: Option<&Timetable>) -> Day` — inside a cached week (date's Monday ∈ `weeks`): School from min `start` / max `end` of its lessons (cancelled lessons included), Off if none; otherwise the unknown-day rule.
  - `pub struct Window { pub start: DateTime<Tz>, pub end: DateTime<Tz>, pub base: Duration }`
  - `pub fn windows_for(tz: Tz, date: NaiveDate, tt: Option<&Timetable>) -> Vec<Window>` — ordered, non-overlapping, per the window table.
  - `pub fn next_scheduled(now: DateTime<Tz>, tt: Option<&Timetable>, rng: &mut StdRng) -> DateTime<Tz>`

`next_scheduled` algorithm (not determined by signature):

```
w = window of today containing now
if w exists and now + jittered(w.base, 0.25) < w.end: return that
for date in today, today+1, ... (max 8 days):
    for w in windows_for(date) where w.start > now: return w.start + between(0, 15 min)
```

- [ ] **Step 1: Write failing tests** (Warsaw tz; build `Timetable` fixtures inline with a helper `tt(days: &[(NaiveDate, &[(h,m,h,m)])], weeks: &[NaiveDate])`)

- `school_day_windows`: Wed 2026-11-25 lessons 08:00–14:30, Thu also school → windows `[06:00–08:00/45, 08:00–14:30/210, 14:30–18:00/150, 18:00–22:00/80]`.
- `friday_evening_is_sparse`: Fri school, Sat off (in cached week) → evening base 240.
- `sunday_evening_is_dense`: Sun 2026-11-29 off, Mon 2026-11-30 school (next cached week) → `[09:00–18:00/300, 18:00–22:00/80]`.
- `late_lessons_skip_afternoon`: lessons until 18:40 → no afternoon window; evening 18:40–22:00.
- `outside_cached_weeks_is_unknown`: cache weeks = [2026-11-23] only; `day_for(2026-12-07)` (Mon) = `School{08:00,15:00}`; `day_for(2026-12-05)` (Sat) = `Off`; `day_for(None)` behaves the same.
- `night_jumps_to_morning_rush`: now 22:10 Wed, Thu first lesson 07:10 → result in [05:10, 05:25).
- `in_window_jitter`: now 06:00 in rush → result in [06:33:45, 06:56:15].
- `candidate_past_window_end_moves_to_next_start`: now 07:50 in rush → result in [08:00, 08:15).
- `dst_spring_forward_day_has_runs`: 2026-03-29 (Sunday, off; Mon school) → windows non-empty, every window start/end has hour ≥ 9 local.
- `week_simulation`: seed 7, cached two weeks (Mon–Fri 08:00–14:30), start Mon 2026-11-23 00:00, iterate `next_scheduled` until Mon 2026-11-30 00:00 → every school day has 8..=11 runs, Sat and Sun each have 3..=5, no run with local time in [22:00, 05:00).

- [ ] **Step 2: Run and verify they fail**

Run: `cargo test family::polling::windows`
Expected: compile errors.

- [ ] **Step 3: Implement `windows.rs`** with the window constants as named `const`s at the top of the file.

- [ ] **Step 4: Run tests**

Run: `cargo test family::polling::windows`
Expected: all pass. If `week_simulation` counts fall outside 8..=11, adjust nothing but report: counts are a spec goal, not a tuning knob for the implementer.

- [ ] **Step 5: Commit**

```bash
git add src/family
git commit -m "feat(family): derive Librus polling windows from the timetable"
```

### Task 4: TimetablePolicy with retries and kid gap

**Files:**
- Create: `src/family/polling/timetable_policy.rs`
- Modify: `src/family/polling/mod.rs`

**Interfaces:**
- Consumes: `PollPolicy`, `Outcome`, `jitter`, `backoff_delay`, `next_scheduled`, `SourceCache<Timetable>`.
- Produces:
  - `#[derive(Clone, Default)] pub struct PlannedRuns(Arc<Mutex<Vec<Option<DateTime<Tz>>>>>)` with `pub fn new(kids: usize) -> Self`
  - `pub struct TimetablePolicy` with `pub fn new(index: usize, cache: Arc<FamilyCaches>, planned: PlannedRuns, rng: StdRng) -> Self`; implements `PollPolicy`.
  - Internal state: `retries: u32`, `scheduled: Option<DateTime<Tz>>`.
  - Pure helper (testable without randomness in the schedule): `fn after_run(now: DateTime<Tz>, scheduled: DateTime<Tz>, retries: u32, outcome: Outcome, rng: &mut StdRng) -> (DateTime<Tz>, u32)` — returns the next time and the retry count to carry.

Behaviour:
- `first_run`: cache for `index` empty → `now + between(1, 8 min) + index × 5 min`; else `next_scheduled(now, ..)`. Then apply the gap rule and record in `planned[index]`.
- `next_run`: if the finished run was not a retry, `scheduled = next_scheduled(now, ..)`. On `Failed` with `retries < 2` and `now + jittered(backoff_delay(retries), 0.25) < scheduled` → return that, `retries += 1`. Otherwise return `scheduled`, `retries = 0`. Apply the gap rule to every returned time and record it.
- Gap rule: while any other kid's planned run is within 4 min of the candidate, candidate += `between(4, 8 min)` (max 5 iterations).

- [ ] **Step 1: Write failing tests** (temp-dir `FamilyCaches`, seeded rng)

- `empty_cache_first_run_is_staggered_by_kid`: kid 0 in [now+1, now+8) min, kid 1 in [now+6, now+13) min.
- `restart_at_night_waits_for_morning`: cache populated (Thu school 08:00), now Wed 03:00 → first run in [06:00, 06:15).
- `failure_retries_twice_then_waits`: now 06:00 Wed in rush: `Failed` → ~5 min (3:45–6:15), `Failed` → ~10 min, `Failed` → the scheduled run (≥ 06:33).
- `retry_never_reaches_scheduled_run`: `after_run(now, now + 2 min, 0, Failed, ..)` → `(now + 2 min, 0)`.
- `rejected_never_retries`: `Rejected` → scheduled run, retries reset.
- `kids_keep_four_minutes_apart`: two policies sharing `PlannedRuns`, 200 seeded `next_run(Ok)` steps each from the same `now` sequence → every pair of planned times ≥ 4 min apart at each step.

- [ ] **Step 2: Run and verify they fail**

Run: `cargo test family::polling::timetable_policy`
Expected: compile errors.

- [ ] **Step 3: Implement**

- [ ] **Step 4: Run tests**

Run: `cargo test family::polling`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add src/family/polling
git commit -m "feat(family): add timetable-aware Librus poll policy with kid staggering"
```

### Task 5: Wire pollers and remove fixed slots

**Files:**
- Modify: `src/family/polling/mod.rs` (add `spawn_family_pollers`), `src/scheduler.rs`, `src/main.rs` if the `spawn_all` signature changes

**Interfaces:**
- Consumes: `run_poller`, `IntervalPolicy`, `TimetablePolicy`, `PlannedRuns`, `scheduler::{Sources, refresh_calendar_once, refresh_weather_once, refresh_timetable_once}`.
- Produces: `pub fn spawn_family_pollers(cfg: Arc<Config>, caches: Arc<FamilyCaches>, sources: Sources, shutdown: CancellationToken) -> Vec<JoinHandle<()>>`; `scheduler::spawn_all` keeps its signature and returns `family::polling::spawn_family_pollers(..)`.
- `fn to_outcome(r: &Result<(), SourceError>) -> Outcome` in `family/polling/mod.rs`: `Ok` → `Ok`, `CredentialsRejected` → `Rejected`, other → `Failed`.

- [ ] **Step 1: Write failing test** `to_outcome_maps_rejected_credentials` in `family/polling/mod.rs`.

- [ ] **Step 2: Implement wiring**; delete from `scheduler.rs`: `CALENDAR_EVERY`, `WEATHER_EVERY`, `SLOT_HOURS`, `next_librus_slot`, `slot_at`, `every`, `MAX_RETRIES_PER_SLOT`, `librus_next_wait`, `librus_loop`, and their tests (`librus_slots*`, `rejected_credentials_are_never_retried_in_the_slot`, `retries_follow_backoff_*`, `retry_is_capped_*`, `success_waits_*`, `failed_last_slot_*`, `never_more_than_two_retries_per_slot`) — their behaviour is now covered by Task 4 tests. Calendar 15 min / weather 30 min constants move to `family/polling/mod.rs`. Each policy gets `rand::make_rng::<StdRng>()`.

- [ ] **Step 3: Full verification**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: clean, all pass. Also: `grep -rnE "crate::(family|sources|dashboards)" src/polling src/cache.rs` prints nothing.

- [ ] **Step 4: Manual check**

Run the server against a real config (`RUST_LOG=info`) for a few minutes; confirm from the `next run at` log lines that calendar/weather fire within 2 min and that Librus does not log in immediately when its cache is present.

- [ ] **Step 5: Commit**

```bash
git add src
git commit -m "feat: poll Librus on timetable-aware jittered schedule"
```
