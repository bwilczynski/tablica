//! Librus polling windows for a day, derived from the cached timetable.

use std::time::Duration;

use chrono::{DateTime, Datelike, Days, NaiveDate, NaiveTime, TimeZone, Weekday};
use chrono_tz::Tz;
use rand::rngs::StdRng;

use crate::polling::{between, jittered};
use crate::sources::librus::timetable::Timetable;

const fn hm(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).expect("valid time")
}

const fn mins(m: u64) -> Duration {
    Duration::from_secs(m * 60)
}

/// How long before the first lesson the morning rush starts.
const RUSH_LEAD: chrono::Duration = chrono::Duration::hours(2);
const RUSH_EVERY: Duration = mins(45);
const SCHOOL_EVERY: Duration = mins(210);
const AFTERNOON_EVERY: Duration = mins(150);
const DAY_OFF_EVERY: Duration = mins(300);
const EVENING_BEFORE_SCHOOL_EVERY: Duration = mins(80);
const EVENING_BEFORE_DAY_OFF_EVERY: Duration = mins(240);

const DAY_OFF_FROM: NaiveTime = hm(9, 0);
const EVENING_FROM: NaiveTime = hm(18, 0);
const NIGHT_FROM: NaiveTime = hm(22, 0);

/// Assumed lesson hours on a weekday the timetable does not cover.
const UNKNOWN_FIRST: NaiveTime = hm(8, 0);
const UNKNOWN_LAST: NaiveTime = hm(15, 0);

/// Spread of a run time inside a window.
const IN_WINDOW_SPREAD: f64 = 0.25;
/// A window's first run lands up to this long after the window opens.
const WINDOW_START_OFFSET: Duration = mins(15);
/// Two consecutive runs are never closer than this.
const MIN_GAP: Duration = mins(15);
/// How far ahead to look for the next window.
const LOOKAHEAD_DAYS: u64 = 8;

/// A kid's school day, as far as polling cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Day {
    School { first: NaiveTime, last: NaiveTime },
    Off,
}

/// The kind of `date`: from the timetable when it covers the date's week,
/// otherwise a guess (Mon–Fri school 08:00–15:00, weekends off).
pub fn day_for(date: NaiveDate, tt: Option<&Timetable>) -> Day {
    let monday = date.week(Weekday::Mon).first_day();
    match tt {
        Some(tt) if tt.weeks.contains(&monday) => {
            let lessons = tt.days.get(&date).map(Vec::as_slice).unwrap_or_default();
            let first = lessons.iter().map(|l| l.start).min();
            let last = lessons.iter().map(|l| l.end).max();
            match (first, last) {
                (Some(first), Some(last)) => Day::School { first, last },
                _ => Day::Off,
            }
        }
        _ => match date.weekday() {
            Weekday::Sat | Weekday::Sun => Day::Off,
            _ => Day::School {
                first: UNKNOWN_FIRST,
                last: UNKNOWN_LAST,
            },
        },
    }
}

/// A stretch of local time polled roughly every `base`.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub start: DateTime<Tz>,
    pub end: DateTime<Tz>,
    pub base: Duration,
}

/// The polling windows of `date`, ordered and non-overlapping. Nights have none.
pub fn windows_for(tz: Tz, date: NaiveDate, tt: Option<&Timetable>) -> Vec<Window> {
    let tomorrow = date.checked_add_days(Days::new(1)).unwrap_or(date);
    let evening_every = match day_for(tomorrow, tt) {
        Day::School { .. } => EVENING_BEFORE_SCHOOL_EVERY,
        Day::Off => EVENING_BEFORE_DAY_OFF_EVERY,
    };
    let at = |t: NaiveTime| tz.from_local_datetime(&date.and_time(t)).earliest();
    let mut out = Vec::new();
    let mut push = |start: Option<DateTime<Tz>>, end: Option<DateTime<Tz>>, base| {
        if let (Some(start), Some(end)) = (start, end)
            && start < end
        {
            out.push(Window { start, end, base });
        }
    };
    match day_for(date, tt) {
        Day::School { first, last } => {
            let rush = tz
                .from_local_datetime(&(date.and_time(first) - RUSH_LEAD))
                .earliest();
            push(rush, at(first), RUSH_EVERY);
            push(at(first), at(last), SCHOOL_EVERY);
            push(at(last), at(EVENING_FROM), AFTERNOON_EVERY);
            push(at(last.max(EVENING_FROM)), at(NIGHT_FROM), evening_every);
        }
        Day::Off => {
            push(at(DAY_OFF_FROM), at(EVENING_FROM), DAY_OFF_EVERY);
            push(at(EVENING_FROM), at(NIGHT_FROM), evening_every);
        }
    }
    out
}

/// True when `at` falls inside one of its day's polling windows.
pub fn in_window(at: DateTime<Tz>, tt: Option<&Timetable>) -> bool {
    windows_for(at.timezone(), at.date_naive(), tt)
        .iter()
        .any(|w| w.start <= at && at < w.end)
}

/// The next Librus run after `now`.
///
/// A jittered interval is spent at the rate of the window it passes through:
/// whatever is left at a window's end carries into the next adjoining window at
/// that window's interval, so a boundary never causes a burst. Across a gap
/// (the night) the run lands shortly after the next window opens.
pub fn next_scheduled(now: DateTime<Tz>, tt: Option<&Timetable>, rng: &mut StdRng) -> DateTime<Tz> {
    let tz = now.timezone();
    let today = now.date_naive();
    let windows = (0..LOOKAHEAD_DAYS)
        .filter_map(|offset| today.checked_add_days(Days::new(offset)))
        .flat_map(|date| windows_for(tz, date, tt))
        .filter(|w| w.end > now);
    // Share of a base interval still to spend before the run.
    let mut budget = jittered(Duration::from_secs(1), IN_WINDOW_SPREAD, rng).as_secs_f64();
    let mut at = now;
    for w in windows {
        if w.start > at {
            let run = w.start + between(Duration::ZERO, WINDOW_START_OFFSET, rng);
            return run.max(now + MIN_GAP);
        }
        at = at.max(w.start);
        let room = (w.end - at).to_std().unwrap_or_default().as_secs_f64() / w.base.as_secs_f64();
        if budget <= room {
            let run = at + w.base.mul_f64(budget);
            return run.max(now + MIN_GAP);
        }
        budget -= room;
        at = w.end;
    }
    now + DAY_OFF_EVERY
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use chrono::{Datelike, NaiveTime, TimeZone, Timelike, Weekday};
    use rand::SeedableRng;

    use crate::sources::librus::timetable::Lesson;

    const WARSAW: Tz = chrono_tz::Europe::Warsaw;
    const MIN: Duration = Duration::from_secs(60);

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn local(y: i32, mo: u32, day: u32, h: u32, mi: u32) -> DateTime<Tz> {
        WARSAW.with_ymd_and_hms(y, mo, day, h, mi, 0).unwrap()
    }

    fn lesson(start: NaiveTime, end: NaiveTime) -> Lesson {
        Lesson {
            start,
            end,
            subject: "x".into(),
            teacher: None,
            room: None,
            cancelled: false,
            flag: None,
        }
    }

    /// A timetable covering `weeks` with lessons from `start` to `end` on `school` dates.
    fn tt(weeks: &[NaiveDate], school: &[(NaiveDate, NaiveTime, NaiveTime)]) -> Timetable {
        let mut days = BTreeMap::new();
        for &(date, start, end) in school {
            days.insert(date, vec![lesson(start, t(9, 0)), lesson(t(9, 0), end)]);
        }
        Timetable {
            weeks: weeks.to_vec(),
            days,
        }
    }

    /// Mon–Fri 08:00–14:30 for the weeks starting at each Monday.
    fn weekdays(mondays: &[NaiveDate]) -> Timetable {
        let school: Vec<_> = mondays
            .iter()
            .flat_map(|m| (0..5).map(move |i| (*m + chrono::Days::new(i), t(8, 0), t(14, 30))))
            .collect();
        tt(mondays, &school)
    }

    fn shape(ws: &[Window]) -> Vec<(String, String, u64)> {
        ws.iter()
            .map(|w| {
                (
                    w.start.format("%H:%M").to_string(),
                    w.end.format("%H:%M").to_string(),
                    w.base.as_secs() / 60,
                )
            })
            .collect()
    }

    fn s(a: &str, b: &str, m: u64) -> (String, String, u64) {
        (a.into(), b.into(), m)
    }

    const MON: NaiveDate = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
    const NEXT_MON: NaiveDate = NaiveDate::from_ymd_opt(2026, 11, 30).unwrap();

    #[test]
    fn school_day_windows() {
        let tt = weekdays(&[MON]);
        assert_eq!(
            shape(&windows_for(WARSAW, d(2026, 11, 25), Some(&tt))),
            vec![
                s("06:00", "08:00", 45),
                s("08:00", "14:30", 210),
                s("14:30", "18:00", 150),
                s("18:00", "22:00", 80),
            ]
        );
    }

    #[test]
    fn friday_evening_is_sparse() {
        let tt = weekdays(&[MON]);
        let ws = windows_for(WARSAW, d(2026, 11, 27), Some(&tt));
        assert_eq!(ws.last().map(|w| w.base), Some(240 * MIN));
    }

    #[test]
    fn sunday_evening_is_dense() {
        let tt = weekdays(&[MON, NEXT_MON]);
        assert_eq!(
            shape(&windows_for(WARSAW, d(2026, 11, 29), Some(&tt))),
            vec![s("09:00", "18:00", 300), s("18:00", "22:00", 80)]
        );
    }

    #[test]
    fn late_lessons_skip_afternoon() {
        let wed = d(2026, 11, 25);
        let tt = tt(
            &[MON],
            &[
                (wed, t(8, 0), t(18, 40)),
                (d(2026, 11, 26), t(8, 0), t(14, 0)),
            ],
        );
        assert_eq!(
            shape(&windows_for(WARSAW, wed, Some(&tt))),
            vec![
                s("06:00", "08:00", 45),
                s("08:00", "18:40", 210),
                s("18:40", "22:00", 80),
            ]
        );
    }

    #[test]
    fn outside_cached_weeks_is_unknown() {
        let tt = weekdays(&[MON]);
        let fallback = Day::School {
            first: t(8, 0),
            last: t(15, 0),
        };
        assert_eq!(day_for(d(2026, 12, 7), Some(&tt)), fallback);
        assert_eq!(day_for(d(2026, 12, 5), Some(&tt)), Day::Off);
        assert_eq!(day_for(d(2026, 12, 7), None), fallback);
        assert_eq!(day_for(d(2026, 12, 5), None), Day::Off);
    }

    #[test]
    fn weekday_without_lessons_in_cached_week_is_off() {
        let tt = tt(&[MON], &[(MON, t(8, 0), t(14, 0))]);
        assert_eq!(day_for(d(2026, 11, 24), Some(&tt)), Day::Off);
        assert_eq!(
            day_for(MON, Some(&tt)),
            Day::School {
                first: t(8, 0),
                last: t(14, 0)
            }
        );
    }

    #[test]
    fn night_jumps_to_morning_rush() {
        let thu = d(2026, 11, 26);
        let tt = tt(
            &[MON],
            &[
                (d(2026, 11, 25), t(8, 0), t(14, 0)),
                (thu, t(7, 10), t(14, 0)),
            ],
        );
        for seed in 0..50 {
            let mut rng = StdRng::seed_from_u64(seed);
            let next = next_scheduled(local(2026, 11, 25, 22, 10), Some(&tt), &mut rng);
            assert!(
                next >= local(2026, 11, 26, 5, 10) && next < local(2026, 11, 26, 5, 25),
                "{next}"
            );
        }
    }

    #[test]
    fn in_window_jitter() {
        let tt = weekdays(&[MON]);
        let lo = local(2026, 11, 25, 6, 33) + Duration::from_secs(45);
        let hi = local(2026, 11, 25, 6, 56) + Duration::from_secs(15);
        for seed in 0..50 {
            let mut rng = StdRng::seed_from_u64(seed);
            let next = next_scheduled(local(2026, 11, 25, 6, 0), Some(&tt), &mut rng);
            assert!(next >= lo && next <= hi, "{next}");
        }
    }

    #[test]
    fn interval_crossing_a_boundary_continues_at_the_next_rate() {
        // 10 of 45 rush minutes are left at 07:50; the rest of a 0.75–1.25
        // interval is spent at the school rate of 210 min.
        let tt = weekdays(&[MON]);
        let lo = local(2026, 11, 25, 8, 0) + (SCHOOL_EVERY.mul_f64(0.75 - 10.0 / 45.0));
        let hi = local(2026, 11, 25, 8, 0) + (SCHOOL_EVERY.mul_f64(1.25 - 10.0 / 45.0));
        for seed in 0..50 {
            let mut rng = StdRng::seed_from_u64(seed);
            let next = next_scheduled(local(2026, 11, 25, 7, 50), Some(&tt), &mut rng);
            assert!(next >= lo && next <= hi, "{next}");
        }
    }

    #[test]
    fn dst_spring_forward_day_has_runs() {
        let tt = weekdays(&[d(2026, 3, 23), d(2026, 3, 30)]);
        let ws = windows_for(WARSAW, d(2026, 3, 29), Some(&tt));
        assert!(!ws.is_empty());
        assert!(ws.iter().all(|w| w.start.hour() >= 9 && w.end.hour() >= 9));
        let mut rng = StdRng::seed_from_u64(1);
        let next = next_scheduled(local(2026, 3, 29, 1, 0), Some(&tt), &mut rng);
        assert!(next.hour() >= 9, "{next}");
    }

    #[test]
    fn week_simulation() {
        for seed in 0..100 {
            simulate_week(seed);
        }
    }

    fn simulate_week(seed: u64) {
        let tt = weekdays(&[MON, NEXT_MON]);
        let mut rng = StdRng::seed_from_u64(seed);
        let end = local(2026, 11, 30, 0, 0);
        let mut now = local(2026, 11, 23, 0, 0);
        let mut per_day: BTreeMap<NaiveDate, u32> = BTreeMap::new();
        loop {
            now = next_scheduled(now, Some(&tt), &mut rng);
            if now >= end {
                break;
            }
            let h = now.hour();
            assert!((5..22).contains(&h), "night run at {now}");
            *per_day.entry(now.date_naive()).or_default() += 1;
        }
        for (date, n) in &per_day {
            match date.weekday() {
                Weekday::Sat => assert!((3..=5).contains(n), "seed {seed} {date}: {n}"),
                Weekday::Sun => assert!((3..=8).contains(n), "seed {seed} {date}: {n}"),
                // Friday's evening is sparse: Saturday is off.
                Weekday::Fri => assert!((6..=11).contains(n), "seed {seed} {date}: {n}"),
                _ => assert!((8..=11).contains(n), "seed {seed} {date}: {n}"),
            }
        }
        assert_eq!(per_day.len(), 7);
    }
}
