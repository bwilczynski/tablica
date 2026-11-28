//! Battery percentage and remaining-days estimate from voltage readings.

use chrono::{DateTime, Duration, Utc};

use crate::history::Reading;

const CURVE: [(f64, f64); 11] = [
    (3.30, 0.0),
    (3.50, 5.0),
    (3.60, 12.0),
    (3.65, 20.0),
    (3.70, 30.0),
    (3.75, 40.0),
    (3.80, 50.0),
    (3.90, 65.0),
    (4.00, 80.0),
    (4.10, 90.0),
    (4.20, 100.0),
];

/// Battery percentage for a cell voltage: piecewise linear, clamped to 0..=100.
pub fn percent(volts: f64) -> f64 {
    let (lo_v, lo_p) = CURVE[0];
    let (hi_v, hi_p) = CURVE[CURVE.len() - 1];
    if volts <= lo_v {
        return lo_p;
    }
    if volts >= hi_v {
        return hi_p;
    }
    for pair in CURVE.windows(2) {
        let ((v0, p0), (v1, p1)) = (pair[0], pair[1]);
        if volts <= v1 {
            return p0 + (p1 - p0) * (volts - v0) / (v1 - v0);
        }
    }
    hi_p
}

#[derive(Debug, Clone, PartialEq)]
pub enum Estimate {
    Collecting { hours: i64 },
    DaysLeft(f64),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Power {
    NoData,
    NoBattery,
    Battery {
        volts: f64,
        percent: f64,
        estimate: Estimate,
    },
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

/// The latest check-in: its time and the voltage as sent.
pub type LatestBattery = (DateTime<Utc>, Option<f64>);

/// Non-zero voltage samples of `readings`, in input order.
pub fn samples_from(readings: &[Reading]) -> Vec<(DateTime<Utc>, f64)> {
    readings
        .iter()
        .filter_map(|r| r.battery_v.filter(|v| *v != 0.0).map(|v| (r.at, v)))
        .collect()
}

pub fn latest_from(r: Option<&Reading>) -> Option<LatestBattery> {
    r.map(|r| (r.at, r.battery_v))
}

/// Battery state from the latest check-in and the recent samples (non-zero
/// volts, sorted by time).
pub fn power(latest: Option<LatestBattery>, samples: &[(DateTime<Utc>, f64)]) -> Power {
    if latest.is_some_and(|(_, v)| v == Some(0.0)) {
        return Power::NoBattery;
    }
    let mut samples = samples.to_vec();
    if let Some((at, v)) = latest
        && let Some(v) = v.filter(|v| *v != 0.0)
        && !samples.iter().any(|x| x.0 == at)
    {
        samples.push((at, v));
    }
    samples.sort_by_key(|s| s.0);
    if samples.is_empty() {
        return Power::NoData;
    }

    let tail = &samples[samples.len().saturating_sub(4)..];
    let volts = median(tail.iter().map(|s| s.1).collect());
    let current = percent(volts);

    let points: Vec<_> = samples.iter().map(|(at, v)| (*at, percent(*v))).collect();
    // A recharge is a rise of the trailing median of three readings, so one odd
    // reading cannot restart the window; it takes two in a row. The rise in the
    // median at `i` means the raw jump began one reading earlier.
    let pcts: Vec<f64> = points.iter().map(|p| p.1).collect();
    let smoothed: Vec<f64> = (0..pcts.len())
        .map(|i| median(pcts[i.saturating_sub(2)..=i].to_vec()))
        .collect();
    let start = (1..smoothed.len())
        .rev()
        .find(|&i| smoothed[i] - smoothed[i - 1] >= 10.0)
        .map_or(0, |i| i - 1);
    let window = &points[start..];
    let first_at = window[0].0;
    let span = window[window.len() - 1].0 - first_at;
    let collecting = Estimate::Collecting {
        hours: span.num_hours(),
    };

    let estimate = if span < Duration::hours(48) {
        collecting
    } else {
        let day = Duration::days(1).num_milliseconds() as f64;
        let xs: Vec<f64> = window
            .iter()
            .map(|(at, _)| (*at - first_at).num_milliseconds() as f64 / day)
            .collect();
        let n = xs.len() as f64;
        let mean_x = xs.iter().sum::<f64>() / n;
        let mean_y = window.iter().map(|p| p.1).sum::<f64>() / n;
        let sxx: f64 = xs.iter().map(|x| (x - mean_x).powi(2)).sum();
        let sxy: f64 = xs
            .iter()
            .zip(window)
            .map(|(x, p)| (x - mean_x) * (p.1 - mean_y))
            .sum();
        let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
        if slope >= 0.0 {
            collecting
        } else {
            Estimate::DaysLeft(current / -slope)
        }
    };

    Power::Battery {
        volts,
        percent: current,
        estimate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap()
    }

    fn reading(hours_ago: i64, v: Option<f64>) -> Reading {
        Reading {
            at: now() - Duration::hours(hours_ago),
            battery_v: v,
            ..Reading::default()
        }
    }

    fn series(hours: i64, f: impl Fn(i64) -> f64) -> Vec<Reading> {
        (0..=hours)
            .rev()
            .map(|h| reading(h, Some(f(hours - h))))
            .collect()
    }

    #[test]
    fn curve_points_and_clamping() {
        for (v, p) in CURVE {
            assert!((percent(v) - p).abs() < 1e-9, "{v}");
        }
        assert!((percent(3.95) - 72.5).abs() < 1e-9);
        assert_eq!(percent(4.5), 100.0);
        assert_eq!(percent(3.0), 0.0);
    }

    #[test]
    fn median_smooths_a_spike() {
        // The two older readings would pull a median over all six down to 3.70.
        let r = vec![
            reading(5, Some(3.50)),
            reading(4, Some(3.50)),
            reading(3, Some(3.90)),
            reading(2, Some(3.91)),
            reading(1, Some(3.50)),
            reading(0, Some(3.90)),
        ];
        match power(latest_from(r.last()), &samples_from(&r)) {
            Power::Battery { volts, .. } => assert!((volts - 3.90).abs() < 1e-9),
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn steady_drain_gives_days_left() {
        let r = series(96, |h| 4.00 - 0.20 * h as f64 / 96.0);
        match power(latest_from(r.last()), &samples_from(&r)) {
            Power::Battery {
                estimate: Estimate::DaysLeft(d),
                ..
            } => assert!((d - 6.7).abs() < 0.5, "{d}"),
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn too_little_history_is_collecting() {
        let r = series(30, |h| 4.00 - 0.01 * h as f64 / 30.0);
        match power(latest_from(r.last()), &samples_from(&r)) {
            Power::Battery { estimate, .. } => {
                assert_eq!(estimate, Estimate::Collecting { hours: 30 })
            }
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn flat_or_rising_is_collecting() {
        let r = series(96, |_| 3.90);
        assert!(matches!(
            power(latest_from(r.last()), &samples_from(&r)),
            Power::Battery {
                estimate: Estimate::Collecting { .. },
                ..
            }
        ));
    }

    #[test]
    fn recharge_restarts_window() {
        // 86 h of drain, then a recharge followed by 10 h of full readings.
        let mut r = series(96, |h| 4.00 - 0.20 * h as f64 / 96.0);
        r.truncate(r.len() - 11);
        let full = |h: i64| reading(h, Some(4.20 - 0.001 * (10 - h) as f64));
        r.extend((0..=10).rev().map(full));
        match power(latest_from(r.last()), &samples_from(&r)) {
            Power::Battery { estimate, .. } => {
                assert_eq!(estimate, Estimate::Collecting { hours: 10 })
            }
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn single_low_reading_does_not_restart_window() {
        let mut r = series(96, |h| 4.00 - 0.20 * h as f64 / 96.0);
        r[48].battery_v = Some(3.40);
        match power(latest_from(r.last()), &samples_from(&r)) {
            Power::Battery {
                estimate: Estimate::DaysLeft(d),
                ..
            } => assert!((d - 6.7).abs() < 1.0, "{d}"),
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn single_high_reading_does_not_restart_window() {
        let mut r = series(96, |h| 4.00 - 0.20 * h as f64 / 96.0);
        r[95].battery_v = Some(4.20);
        assert!(matches!(
            power(latest_from(r.last()), &samples_from(&r)),
            Power::Battery {
                estimate: Estimate::DaysLeft(_),
                ..
            }
        ));
    }

    #[test]
    fn zero_volts() {
        let r = vec![reading(1, Some(3.9)), reading(0, Some(0.0))];
        assert_eq!(
            power(latest_from(r.last()), &samples_from(&r)),
            Power::NoBattery
        );

        let mut r = series(96, |h| 4.00 - 0.20 * h as f64 / 96.0);
        r[40].battery_v = Some(0.0);
        assert!(matches!(
            power(latest_from(r.last()), &samples_from(&r)),
            Power::Battery {
                estimate: Estimate::DaysLeft(_),
                ..
            }
        ));

        let r = vec![
            reading(3, Some(3.9)),
            reading(2, Some(0.0)),
            reading(1, Some(3.9)),
        ];
        let latest = reading(0, None);
        assert!(matches!(
            power(latest_from(Some(&latest)), &samples_from(&r)),
            Power::Battery { .. }
        ));

        assert_eq!(power(latest_from(None), &samples_from(&[])), Power::NoData);
        assert_eq!(
            power(latest_from(Some(&latest)), &samples_from(&[])),
            Power::NoData
        );
    }

    #[test]
    fn latest_zero_then_missing() {
        let r = series(10, |_| 3.9);
        let t = now() + Duration::hours(1);
        assert_eq!(
            power(Some((t, Some(0.0))), &samples_from(&r)),
            Power::NoBattery
        );
        assert!(matches!(
            power(Some((t, None)), &samples_from(&r)),
            Power::Battery { .. }
        ));
    }
}
