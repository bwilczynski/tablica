# Timetable-aware, jittered polling — design

## Intent

Librus should not see a bot polling it on a fixed clock. Today each kid logs
in at exactly 05:30, 07:30 … 21:30 (9 logins a day, weekends included), and
all kids log in at the same instant. We want:

1. **Randomness**: no fixed times, no fixed intervals, kids not back to back.
2. **Uneven frequency** that follows when timetables actually change: the
   ~2 hours before the first lesson, and the evening before a school day.
   Polling is sparse while lessons run and near-silent on days off.
3. **A polling abstraction** so each dashboard can bring its own strategies,
   shaped so that a later split of family and homelab into separate crates
   is mostly moving files, not a redesign.

## Goals

- Librus run times derived from each kid's cached timetable, with jitter.
- Per-kid staggering: kids never start within a few minutes of each other.
- A generic `polling` module (policy trait + runner) with **no imports of
  family types**.
- Family-specific policies and wiring live in family-owned code.
- Daily Librus logins per kid on a school day stay at or below today's 9.

## Non-goals

- Splitting into crates now (that is the follow-up this prepares for).
- Decoupling `device_api` from family types (render path / health endpoint).
  It is the other seam the crate split needs; separate change.
- Making the schedule configurable in TOML. Constants in code; config can be
  added later if tuning is needed.
- Changing calendar/weather cadence beyond adding jitter.

## Anti-goals

- More Librus traffic than today in exchange for freshness.
- A policy that can stall: unknown or broken timetable data must still poll.
- Non-deterministic tests: randomness is injectable and seeded in tests.

## Approaches considered

- **A. Polling abstraction now, module boundaries shaped like future crates
  (chosen).** Generic `polling` module plus family-supplied policies and
  registration. Moderate refactor of `scheduler.rs`; crate split later is
  mechanical.
- **B. Workspace split now.** Settles the boundary but is a large change in a
  feature branch, and homelab's shape is still a guess.
- **C. Strategies picked in config.** Adds config surface without helping the
  split; strategies still have to exist in code.

## Design

### Module layout

```
src/polling/mod.rs      PollPolicy trait, Outcome, run_poller loop
src/polling/jitter.rs   jitter helpers (rand-based, seedable)
src/polling/backoff.rs  backoff_delay (moved from scheduler.rs)
src/family/mod.rs       FamilyCaches (moved from cache.rs `Caches`)
src/family/polling.rs   TimetablePolicy, IntervalPolicy use, spawn_family_pollers
src/scheduler.rs        thin: spawn_all -> family::polling::spawn_family_pollers
```

Rule: `polling` and `cache` (generic `SourceCache`, `Stamped`, `is_stale`)
import nothing from `family`, `sources`, or `dashboards`. Existing source
modules (`sources/librus`, `sources/calendar`, `sources/weather`) and
`dashboards/family.rs` stay where they are for now; they move with the
crate split.

`Caches` → `family::FamilyCaches` (same fields, same `open`); `main.rs` and
`device_api` update their import only. `*_MAX_AGE` constants move with it.

### The abstraction

```rust
pub enum Outcome { Ok, Failed, Rejected }   // Rejected = never retry

pub trait PollPolicy: Send {
    /// When to run next, given the local time just after a run and its outcome.
    fn next_run(&mut self, now: DateTime<Tz>, outcome: Outcome) -> DateTime<Tz>;
    /// Delay before the very first run after startup.
    fn initial_delay(&mut self) -> Duration;
}

pub async fn run_poller<P, F, Fut>(tz: Tz, policy: P, shutdown: CancellationToken, job: F)
where F: FnMut() -> Fut, Fut: Future<Output = Outcome>;
```

`run_poller` replaces both `every` and `librus_loop`: sleep `initial_delay`,
run the job, ask the policy for the next time, sleep, repeat; exits on
shutdown. Policies own their RNG (`rand::rngs::StdRng`, seeded in tests) and
their retry state.

Generic policy shipped in `polling`: **`IntervalPolicy { period, jitter }`**:
next = now + period × uniform(1 − jitter, 1 + jitter); failures use
`backoff_delay` with the same jitter. Calendar (15 min) and weather
(30 min) use it with ±20 %.

### TimetablePolicy (Librus, one instance per kid)

Reads the kid's `SourceCache<Timetable>` on every `next_run`. For a date D:

- **Lessons known:** D is inside the cached `weeks` and has lessons →
  first lesson start `F(D)`, last lesson end `L(D)`.
- **Day off:** D is inside cached `weeks` with no lessons.
- **Unknown:** D outside cached weeks or no cache → assume a school day on
  Mon–Fri with F = 08:00, L = 15:00; day off on Sat/Sun.

Windows for a day D (local time), each with a base interval:

| Window | When | Base interval |
|---|---|---|
| Morning rush | F(D) − 2h … F(D), if D is a school day | 40 min |
| School | F(D) … L(D) | 150 min |
| Afternoon | L(D) … 18:00 (or day start … 18:00 on a day off) | 120 min |
| Evening | 18:00 … 22:00, if D+1 is a school day | 50 min |
| Evening, day off tomorrow | 18:00 … 22:00 otherwise | 180 min |
| Night | 22:00 … morning rush (or 08:00 on a day off) | no runs |

If L(D) is after 18:00 the afternoon window is empty and the evening starts
at L(D). A day off with a school day tomorrow (e.g. Sunday) gets the dense
evening.
A plain day off gets roughly 2–3 runs total.

`next_run(now)`: take the window containing `now`, candidate =
now + base × uniform(0.75, 1.25). If the candidate falls past the window's
end, the next run is the start of the next window that has runs, plus a
random 0–15 min offset (so window boundaries never fire at exact times).
Night is skipped this way. Rough total for a school day: ~3 + 2–3 + 1–2 +
~5 ≈ 9, i.e. no more than today.

Retries: on `Failed`, `backoff_delay(retries)` with ±25 % jitter, at most 2
per scheduled run and never past the next planned run (same rules as
today's `librus_next_wait`). `Rejected` (bad credentials) never retries.

### Staggering

`initial_delay` for kid i = random 1–8 min + i × 5 min, so a restart never
logs both kids in together. After that each kid has its own RNG, and
TimetablePolicy keeps a minimum gap: on every `next_run`, if the result is
within 4 min of another kid's planned run, push it out by 4–8 min. The
policies share an `Arc<Mutex<Vec<Option<DateTime<Tz>>>>>` of planned runs,
created in `spawn_family_pollers`. Calendar/weather start after a random
0–2 min.

### Testing

- Policies are pure given (now, outcome, cached timetable, seeded RNG):
  unit-test windows (school day, day off, Sunday evening, unknown week,
  DST day), jitter bounds, retry cap, night skipping, kid gap.
- Property-style check: simulate a week with a seeded RNG and assert
  per-day run counts and that no run falls in the night.
- Existing `refresh_*_once` tests stay unchanged.

### Dependency

Add `rand` (standard, seedable via `StdRng::seed_from_u64`). New runtime
dependency; `fastrand` was considered (smaller) but `rand` is the familiar choice.

## Builder's calls

- Exact window bounds and intervals above are tuned guesses; constants in
  `family/polling.rs`, easy to adjust.
- Mutex vs. channel for the kid gap: builder's choice, provided it's
  testable without tokio time.
- The `next_run` return type may be `Duration` instead of `DateTime` if that
  makes DST handling simpler; window math must stay in local time.
