//! Librus poll policy: timetable-derived schedule, bounded retries, kids kept apart.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::DateTime;
use chrono_tz::Tz;
use rand::rngs::StdRng;

use super::windows::{in_window, next_scheduled};
use crate::cache::is_stale;
use crate::family::{FamilyCaches, TIMETABLE_MAX_AGE};
use crate::polling::{Outcome, PollPolicy, backoff_delay, between, jittered};
use crate::sources::librus::timetable::Timetable;

/// Retries allowed after a failed run, per scheduled run.
const MAX_RETRIES: u32 = 2;
const RETRY_SPREAD: f64 = 0.25;
/// Kids' runs are never planned closer together than this.
const KID_GAP: Duration = Duration::from_secs(4 * 60);
const KID_PUSH_MIN: Duration = Duration::from_secs(4 * 60);
const KID_PUSH_MAX: Duration = Duration::from_secs(8 * 60);
const KID_PUSH_ATTEMPTS: usize = 5;
/// First run after startup with nothing cached: 1–8 min, plus 5 min per kid.
const COLD_START_MIN: Duration = Duration::from_secs(60);
const COLD_START_MAX: Duration = Duration::from_secs(8 * 60);
const COLD_START_PER_KID: Duration = Duration::from_secs(5 * 60);

/// The next planned run of every kid, shared by their policies.
#[derive(Clone, Default)]
pub struct PlannedRuns(Arc<Mutex<Vec<Option<DateTime<Tz>>>>>);

impl PlannedRuns {
    pub fn new(kids: usize) -> Self {
        Self(Arc::new(Mutex::new(vec![None; kids])))
    }

    /// Moves `at` away from other kids' runs, then records it as `index`'s plan.
    fn claim(&self, index: usize, mut at: DateTime<Tz>, rng: &mut StdRng) -> DateTime<Tz> {
        let mut runs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let gap = chrono::Duration::from_std(KID_GAP).unwrap_or_default();
        for _ in 0..KID_PUSH_ATTEMPTS {
            let clash = runs
                .iter()
                .enumerate()
                .any(|(i, r)| i != index && r.is_some_and(|r| (r - at).abs() < gap));
            if !clash {
                break;
            }
            at += between(KID_PUSH_MIN, KID_PUSH_MAX, rng);
        }
        if let Some(slot) = runs.get_mut(index) {
            *slot = Some(at);
        }
        at
    }
}

/// The time after a run and the retry count to carry: on failure retry with
/// backoff (at most `MAX_RETRIES`, always before `scheduled`), otherwise wait
/// for `scheduled`. Rejected credentials never retry.
pub(crate) fn after_run(
    now: DateTime<Tz>,
    scheduled: DateTime<Tz>,
    retries: u32,
    outcome: Outcome,
    rng: &mut StdRng,
) -> (DateTime<Tz>, u32) {
    if outcome == Outcome::Failed && retries < MAX_RETRIES {
        let retry = now + jittered(backoff_delay(retries), RETRY_SPREAD, rng);
        if retry < scheduled {
            return (retry, retries + 1);
        }
    }
    (scheduled, 0)
}

/// Polls one kid's Librus timetable on the schedule its cached timetable implies.
pub struct TimetablePolicy {
    index: usize,
    caches: Arc<FamilyCaches>,
    planned: PlannedRuns,
    rng: StdRng,
    retries: u32,
    scheduled: Option<DateTime<Tz>>,
}

impl TimetablePolicy {
    pub fn new(index: usize, caches: Arc<FamilyCaches>, planned: PlannedRuns, rng: StdRng) -> Self {
        Self {
            index,
            caches,
            planned,
            rng,
            retries: 0,
            scheduled: None,
        }
    }

    fn timetable(&self) -> Option<crate::cache::Stamped<Timetable>> {
        self.caches.timetables.get(self.index).and_then(|c| c.get())
    }

    fn scheduled_after(&mut self, now: DateTime<Tz>) -> DateTime<Tz> {
        let tt = self.timetable().map(|s| s.data);
        next_scheduled(now, tt.as_ref(), &mut self.rng)
    }

    /// Claims `at` against the other kids; if that pushes it out of its
    /// window (into the night), takes the next scheduled run instead.
    fn place(&mut self, at: DateTime<Tz>) -> DateTime<Tz> {
        let tt = self.timetable().map(|s| s.data);
        let mut at = self.planned.claim(self.index, at, &mut self.rng);
        for _ in 0..KID_PUSH_ATTEMPTS {
            if in_window(at, tt.as_ref()) {
                break;
            }
            let next = next_scheduled(at, tt.as_ref(), &mut self.rng);
            at = self.planned.claim(self.index, next, &mut self.rng);
        }
        at
    }

    /// Nothing cached, or the cache is stale while it is daytime: refresh soon.
    fn needs_cold_start(&self, now: DateTime<Tz>) -> bool {
        match self.timetable() {
            None => true,
            Some(s) => {
                is_stale(
                    s.fetched_at,
                    now.with_timezone(&chrono::Utc),
                    TIMETABLE_MAX_AGE,
                ) && in_window(now, Some(&s.data))
            }
        }
    }
}

impl PollPolicy for TimetablePolicy {
    fn first_run(&mut self, now: DateTime<Tz>) -> DateTime<Tz> {
        let has_data = self.timetable().is_some();
        let at = if self.needs_cold_start(now) {
            let stagger = COLD_START_PER_KID * u32::try_from(self.index).unwrap_or(u32::MAX);
            now + between(COLD_START_MIN, COLD_START_MAX, &mut self.rng) + stagger
        } else {
            self.scheduled_after(now)
        };
        // With nothing cached any time will do; otherwise keep out of the night.
        let at = if has_data {
            self.place(at)
        } else {
            self.planned.claim(self.index, at, &mut self.rng)
        };
        self.scheduled = Some(at);
        at
    }

    fn next_run(&mut self, now: DateTime<Tz>, outcome: Outcome) -> DateTime<Tz> {
        // After a retry the earlier scheduled run still stands.
        let scheduled = match self.scheduled {
            Some(s) if self.retries > 0 && s > now => s,
            _ => self.scheduled_after(now),
        };
        let (at, retries) = after_run(now, scheduled, self.retries, outcome, &mut self.rng);
        let tt = self.timetable().map(|s| s.data);
        let mut placed = None;
        if retries > 0 && in_window(at, tt.as_ref()) {
            let retry = self.planned.claim(self.index, at, &mut self.rng);
            // Keeping kids apart may push a retry into the night or past the
            // regular run; then the regular run is what happens next.
            if retry < scheduled && in_window(retry, tt.as_ref()) {
                placed = Some(retry);
            }
        }
        match placed {
            Some(retry) => {
                self.retries = retries;
                self.scheduled = Some(scheduled);
                retry
            }
            None => {
                let at = self.place(scheduled);
                self.retries = 0;
                self.scheduled = Some(at);
                at
            }
        }
    }

    fn recheck(&mut self, now: DateTime<Tz>) -> Option<DateTime<Tz>> {
        let tt = self.timetable()?.data;
        if in_window(now, Some(&tt)) {
            return None;
        }
        // Woke late into the night: wait for the next window instead.
        let next = next_scheduled(now, Some(&tt), &mut self.rng);
        let at = self.place(next);
        self.retries = 0;
        self.scheduled = Some(at);
        Some(at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use chrono::{NaiveDate, NaiveTime, TimeZone};
    use rand::SeedableRng;

    use crate::sources::librus::timetable::{Lesson, Timetable};

    const WARSAW: Tz = chrono_tz::Europe::Warsaw;
    const MIN: Duration = Duration::from_secs(60);
    const MON: NaiveDate = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();

    fn local(h: u32, m: u32) -> DateTime<Tz> {
        WARSAW.with_ymd_and_hms(2026, 11, 25, h, m, 0).unwrap()
    }

    fn weekdays() -> Timetable {
        let t = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
        let days: BTreeMap<_, _> = (0..5)
            .map(|i| {
                let lesson = Lesson {
                    start: t(8, 0),
                    end: t(14, 30),
                    subject: "x".into(),
                    teacher: None,
                    room: None,
                    cancelled: false,
                    flag: None,
                };
                (MON + chrono::Days::new(i), vec![lesson])
            })
            .collect();
        Timetable {
            weeks: vec![MON],
            days,
        }
    }

    fn caches(dir: &tempfile::TempDir, cached: bool) -> Arc<FamilyCaches> {
        let caches = FamilyCaches::open(dir.path(), 2);
        if cached {
            for c in &caches.timetables {
                c.put(weekdays(), local(5, 0).with_timezone(&chrono::Utc))
                    .unwrap();
            }
        }
        Arc::new(caches)
    }

    fn policy(
        index: usize,
        caches: &Arc<FamilyCaches>,
        planned: &PlannedRuns,
        seed: u64,
    ) -> TimetablePolicy {
        TimetablePolicy::new(
            index,
            caches.clone(),
            planned.clone(),
            StdRng::seed_from_u64(seed),
        )
    }

    #[test]
    fn empty_cache_first_run_is_staggered_by_kid() {
        let dir = tempfile::tempdir().unwrap();
        let caches = caches(&dir, false);
        for seed in 0..50 {
            let now = local(12, 0);
            let k0 = policy(0, &caches, &PlannedRuns::new(2), seed).first_run(now);
            let k1 = policy(1, &caches, &PlannedRuns::new(2), seed).first_run(now);
            assert!(k0 >= now + MIN && k0 < now + 8 * MIN, "{k0}");
            assert!(k1 >= now + 6 * MIN && k1 < now + 13 * MIN, "{k1}");
        }
    }

    #[test]
    fn restart_at_night_waits_for_morning() {
        let dir = tempfile::tempdir().unwrap();
        let caches = caches(&dir, true);
        for seed in 0..50 {
            let first = policy(0, &caches, &PlannedRuns::new(2), seed).first_run(local(3, 0));
            assert!(first >= local(6, 0) && first < local(6, 15), "{first}");
        }
    }

    #[test]
    fn failure_retries_twice_then_waits() {
        let dir = tempfile::tempdir().unwrap();
        let caches = caches(&dir, true);
        let mut p = policy(0, &caches, &PlannedRuns::new(2), 9);
        let r1 = p.next_run(local(6, 0), Outcome::Failed);
        assert!(r1 >= local(6, 3) && r1 <= local(6, 7), "{r1}");
        let r2 = p.next_run(local(6, 5), Outcome::Failed);
        assert!(r2 >= local(6, 12) && r2 <= local(6, 18), "{r2}");
        let r3 = p.next_run(local(6, 15), Outcome::Failed);
        assert!(r3 >= local(6, 33), "{r3}");
        // The retry budget is back for the next scheduled run.
        let r4 = p.next_run(r3, Outcome::Failed);
        assert!(r4 <= r3 + 7 * MIN, "{r4}");
    }

    #[test]
    fn retry_never_reaches_scheduled_run() {
        let mut rng = StdRng::seed_from_u64(1);
        let now = local(7, 58);
        assert_eq!(
            after_run(now, now + 2 * MIN, 0, Outcome::Failed, &mut rng),
            (now + 2 * MIN, 0)
        );
    }

    #[test]
    fn after_run_retries_with_backoff() {
        let mut rng = StdRng::seed_from_u64(1);
        let now = local(6, 0);
        let (at, n) = after_run(now, local(7, 0), 0, Outcome::Failed, &mut rng);
        assert!(at >= now + Duration::from_secs(225) && at <= now + Duration::from_secs(375));
        assert_eq!(n, 1);
        assert_eq!(
            after_run(now, local(7, 0), 2, Outcome::Failed, &mut rng),
            (local(7, 0), 0)
        );
        assert_eq!(
            after_run(now, local(7, 0), 1, Outcome::Ok, &mut rng),
            (local(7, 0), 0)
        );
    }

    #[test]
    fn rejected_never_retries() {
        let dir = tempfile::tempdir().unwrap();
        let caches = caches(&dir, true);
        let mut p = policy(0, &caches, &PlannedRuns::new(2), 3);
        let next = p.next_run(local(6, 0), Outcome::Rejected);
        assert!(next >= local(6, 33), "{next}");
    }

    #[test]
    fn kids_keep_four_minutes_apart() {
        let dir = tempfile::tempdir().unwrap();
        let caches = caches(&dir, true);
        let planned = PlannedRuns::new(2);
        let mut kids = [
            policy(0, &caches, &planned, 1),
            policy(1, &caches, &planned, 1),
        ];
        let mut now = [local(6, 0), local(6, 0)];
        for step in 0..200 {
            let i = step % 2;
            now[i] = kids[i].next_run(now[i], Outcome::Ok);
            if step > 0 {
                let gap = (now[0] - now[1]).abs();
                assert!(gap >= chrono::Duration::minutes(4), "step {step}: {now:?}");
            }
        }
    }

    fn stale_caches(dir: &tempfile::TempDir) -> Arc<FamilyCaches> {
        let caches = FamilyCaches::open(dir.path(), 2);
        let three_days_ago = local(9, 0).with_timezone(&chrono::Utc) - chrono::Duration::days(3);
        caches.timetables[0]
            .put(weekdays(), three_days_ago)
            .unwrap();
        Arc::new(caches)
    }

    #[test]
    fn stale_cache_in_daytime_refreshes_soon() {
        let dir = tempfile::tempdir().unwrap();
        let caches = stale_caches(&dir);
        for seed in 0..50 {
            let now = local(9, 0);
            let first = policy(0, &caches, &PlannedRuns::new(2), seed).first_run(now);
            assert!(first >= now + MIN && first < now + 8 * MIN, "{first}");
        }
    }

    #[test]
    fn stale_cache_at_night_waits_for_morning() {
        let dir = tempfile::tempdir().unwrap();
        let caches = stale_caches(&dir);
        for seed in 0..50 {
            let first = policy(0, &caches, &PlannedRuns::new(2), seed).first_run(local(3, 0));
            assert!(first >= local(6, 0) && first < local(6, 15), "{first}");
        }
    }

    #[test]
    fn late_failure_never_retries_into_the_night() {
        let dir = tempfile::tempdir().unwrap();
        let caches = caches(&dir, true);
        let next_morning = WARSAW.with_ymd_and_hms(2026, 11, 26, 6, 0, 0).unwrap();
        for seed in 0..50 {
            let mut p = policy(0, &caches, &PlannedRuns::new(2), seed);
            let next = p.next_run(local(21, 58), Outcome::Failed);
            assert!(next >= next_morning, "seed {seed}: {next}");
        }
    }

    #[test]
    fn two_kids_never_run_at_night() {
        let dir = tempfile::tempdir().unwrap();
        let caches = caches(&dir, true);
        let planned = PlannedRuns::new(2);
        for seed in 0..20 {
            let mut kids = [
                policy(0, &caches, &planned, seed),
                policy(1, &caches, &planned, seed + 100),
            ];
            let mut outcomes = StdRng::seed_from_u64(seed);
            let mut now = [local(6, 0), local(6, 0)];
            for step in 0..300 {
                let i = step % 2;
                let outcome = if rand::RngExt::random_bool(&mut outcomes, 0.3) {
                    Outcome::Failed
                } else {
                    Outcome::Ok
                };
                now[i] = kids[i].next_run(now[i], outcome);
                let h = chrono::Timelike::hour(&now[i]);
                assert!((5..22).contains(&h), "seed {seed} step {step}: {}", now[i]);
            }
        }
    }

    fn next_morning() -> DateTime<Tz> {
        WARSAW.with_ymd_and_hms(2026, 11, 26, 6, 0, 0).unwrap()
    }

    #[test]
    fn stale_cache_late_evening_start_stays_out_of_the_night() {
        let dir = tempfile::tempdir().unwrap();
        let caches = stale_caches(&dir);
        for seed in 0..50 {
            let first = policy(0, &caches, &PlannedRuns::new(2), seed).first_run(local(21, 59));
            assert!(
                first < local(22, 0) || first >= next_morning(),
                "seed {seed}: {first}"
            );
        }
    }

    #[test]
    fn late_wake_at_night_is_postponed_to_the_morning() {
        let dir = tempfile::tempdir().unwrap();
        let caches = caches(&dir, true);
        let mut p = policy(0, &caches, &PlannedRuns::new(2), 1);
        let later = p
            .recheck(local(23, 30))
            .expect("night run must be postponed");
        assert!(later >= next_morning(), "{later}");
        assert_eq!(p.recheck(local(10, 0)), None);
    }

    #[test]
    fn retry_pushed_past_its_deadline_becomes_the_regular_run() {
        let dir = tempfile::tempdir().unwrap();
        let caches = caches(&dir, true);
        for seed in 0..200 {
            let planned = PlannedRuns::new(2);
            let mut other = StdRng::seed_from_u64(seed);
            planned.claim(1, local(21, 56), &mut other);
            let mut p = policy(0, &caches, &planned, seed);
            let at = p.next_run(local(21, 51), Outcome::Failed);
            if p.retries > 0 {
                assert!(at < local(22, 0), "seed {seed}: retry at {at}");
                assert!(
                    Some(at) < p.scheduled,
                    "seed {seed}: retry {at} not before {:?}",
                    p.scheduled
                );
            } else {
                assert_eq!(Some(at), p.scheduled, "seed {seed}");
            }
        }
    }
}
