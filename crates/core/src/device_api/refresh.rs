//! How long a device sleeps before its next `/api/display` call.

use chrono::{DateTime, Days, NaiveDate, NaiveTime, TimeZone};
use chrono_tz::Tz;

use crate::config::Schedule;

/// Never ask the device to call back sooner than this.
const MIN_SECS: i64 = 60;
/// How long after `switch_at` the device should land.
const AFTER_SWITCH_SECS: i64 = 60;

/// Seconds until the device should call again.
///
/// Inside `[wake, sleep)` this is the refresh cadence, shortened to land a minute
/// after `switch_at` when the switch comes sooner. Outside the window the device
/// sleeps until the next local `wake`. Never less than 60.
pub fn refresh_rate(now: DateTime<Tz>, schedule: &Schedule, switch_at: Option<NaiveTime>) -> u32 {
    let (wake, sleep) = (schedule.wake, schedule.sleep);
    let secs = if in_window(now.time(), wake, sleep) {
        let cadence = i64::from(schedule.refresh_minutes) * 60;
        let to_switch = switch_at
            .filter(|&s| in_window(s, wake, sleep))
            .and_then(|s| local(now.timezone(), now.date_naive(), s))
            .map(|s| (s - now).num_seconds() + AFTER_SWITCH_SECS)
            .filter(|&d| d > 0);
        to_switch.map_or(cadence, |d| d.min(cadence))
    } else {
        (next_wake(now, wake) - now).num_seconds()
    };
    u32::try_from(secs.max(MIN_SECS)).unwrap_or(u32::MAX)
}

/// True when `t` is in `[wake, sleep)`; handles a window that spans midnight.
fn in_window(t: NaiveTime, wake: NaiveTime, sleep: NaiveTime) -> bool {
    if wake <= sleep {
        wake <= t && t < sleep
    } else {
        t >= wake || t < sleep
    }
}

/// `time` on `date` in `tz`; a time skipped by a DST jump moves an hour later.
fn local(tz: Tz, date: NaiveDate, time: NaiveTime) -> Option<DateTime<Tz>> {
    let naive = date.and_time(time);
    tz.from_local_datetime(&naive).earliest().or_else(|| {
        tz.from_local_datetime(&(naive + chrono::Duration::hours(1)))
            .earliest()
    })
}

/// The first local `wake` strictly after `now`.
fn next_wake(now: DateTime<Tz>, wake: NaiveTime) -> DateTime<Tz> {
    let today = now.date_naive();
    (0..3)
        .filter_map(|n| today.checked_add_days(Days::new(n)))
        .filter_map(|d| local(now.timezone(), d, wake))
        .find(|w| *w > now)
        .unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Europe::Warsaw;

    fn time(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn warsaw(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Tz> {
        Warsaw.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    fn schedule() -> Schedule {
        Schedule {
            wake: time(6, 0),
            sleep: time(22, 0),
            refresh_minutes: 15,
        }
    }

    #[test]
    fn refresh_rate_daytime_is_15_min() {
        let s = schedule();
        assert_eq!(
            refresh_rate(warsaw(2026, 11, 25, 10, 0), &s, Some(time(16, 0))),
            900
        );
        assert_eq!(refresh_rate(warsaw(2026, 11, 25, 10, 0), &s, None), 900);
        // Past today's switch: the plain cadence again.
        assert_eq!(
            refresh_rate(warsaw(2026, 11, 25, 16, 5), &s, Some(time(16, 0))),
            900
        );
    }

    #[test]
    fn refresh_rate_lands_after_switch() {
        let s = schedule();
        assert_eq!(
            refresh_rate(warsaw(2026, 11, 25, 15, 50), &s, Some(time(16, 0))),
            660
        );
    }

    #[test]
    fn refresh_rate_night_sleeps_until_wake() {
        let s = schedule();
        assert_eq!(
            refresh_rate(warsaw(2026, 11, 25, 22, 0), &s, Some(time(16, 0))),
            28_800
        );
        assert_eq!(refresh_rate(warsaw(2026, 11, 26, 3, 0), &s, None), 3 * 3600);
    }

    #[test]
    fn refresh_rate_across_dst_wakes_at_local_six() {
        let now = warsaw(2026, 10, 24, 22, 0);
        assert_eq!(now.to_rfc3339(), "2026-10-24T22:00:00+02:00");
        assert_eq!(refresh_rate(now, &schedule(), Some(time(16, 0))), 32_400);
    }

    #[test]
    fn refresh_rate_is_at_least_a_minute() {
        let s = Schedule {
            refresh_minutes: 0,
            ..schedule()
        };
        assert_eq!(refresh_rate(warsaw(2026, 11, 25, 10, 0), &s, None), 60);
        assert_eq!(
            refresh_rate(warsaw(2026, 11, 26, 5, 59), &schedule(), None),
            60
        );
    }
}
