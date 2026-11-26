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
- Daily Librus logins per kid on a school day stay around today's 9, about 11 at most.
  Not a hard cap: unlucky jitter or retries after failures can add one or two.

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
    /// When to run first after startup.
    fn first_run(&mut self, now: DateTime<Tz>) -> DateTime<Tz>;
}

pub async fn run_poller<P, F, Fut>(tz: Tz, policy: P, shutdown: CancellationToken, job: F)
where F: FnMut() -> Fut, Fut: Future<Output = Outcome>;
```

`run_poller` replaces both `every` and `librus_loop`: sleep until `first_run`,
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
| Morning rush | F(D) − 2h … F(D), if D is a school day | 45 min |
| School | F(D) … L(D) | 210 min |
| Afternoon | L(D) … 18:00 | 150 min |
| Daytime, day off | 09:00 … 18:00 on a day off | 300 min |
| Evening | 18:00 … 22:00, if D+1 is a school day | 80 min |
| Evening, day off tomorrow | 18:00 … 22:00 otherwise | 240 min |
| Night | 22:00 … morning rush (or 09:00 on a day off) | no runs |

If L(D) is after 18:00 the afternoon window is empty and the evening starts
at L(D). A day off with a school day tomorrow (e.g. Sunday) gets the dense
evening.
A plain day off gets roughly 3–5 runs total.

`next_run(now)`: draw an interval factor k = uniform(0.75, 1.25) and spend
it at the rate of the windows it passes through: inside a window, k × base;
whatever is left at a window's end carries into the adjoining window at that
window's base. Only across a gap (the night) does the run snap to the next
window's start plus a random 0–15 min offset. Runs are never closer than
15 min. Expected runs per day are the sum of window length / base: Mon–Thu
8–10, Fri 6–8 (sparse evening), Sat 3–4, Sun 4–6 (dense evening).
(Changed during implementation: the first rule, "a candidate past the window
end jumps to the next window's start", put a run at every window boundary,
which produced bursts like 07:42 then 08:07 and up to 12 runs a day.)

Retries: on `Failed`, `backoff_delay(retries)` with ±25 % jitter, at most 2
per scheduled run and never past the next planned run (same rules as
today's `librus_next_wait`). `Rejected` (bad credentials) never retries.

With a timetable cached, no Librus run starts at night: first runs, retries
and kid-gap pushes all stay inside windows. A retry pushed into the night or
past the regular run is dropped in favour of the regular run. The runner asks
the policy once more right before a run (`PollPolicy::recheck`); a run due at
night, e.g. after the timer woke late, is postponed to the next window.

### Staggering

`first_run` for kid i:
- cache empty (e.g. a fresh container): fetch at startup, at any hour. The
  kids with nothing cached go in config order, the first right away and each
  next one 20–40 s after the previous (a one-off exception to the 4-min kid
  gap: the dashboard has nothing to show until then). These times are
  reserved before any poller starts, so kids with cached data keep their
  4-min gap from them.
- cache stale (older than 26 h) during the day: now + random 1–8 min +
  i × 5 min.
- otherwise: the next scheduled run, so a restart at 03:00 does not log in
  at 03:00.

After that each kid has its own RNG, and
TimetablePolicy keeps a minimum gap: on every `next_run`, if the result is
within 4 min of another kid's planned run, push it out by 4–8 min. The
policies share an `Arc<Mutex<Vec<Option<DateTime<Tz>>>>>` of planned runs,
created in `spawn_family_pollers`. Calendar/weather fetch right away at
startup (changed after release: the 0–2 min start jitter left the dashboard
showing "unavailable" on a fresh container).

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
