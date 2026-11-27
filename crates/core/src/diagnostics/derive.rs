//! Wi-Fi, wake-time and reset-reason status for the diagnostics screen.

use chrono::{DateTime, Duration, Utc};

use super::Status;
use crate::history::Reading;

pub fn rssi_status(dbm: i64) -> (&'static str, Status) {
    if dbm >= -67 {
        ("good", Status::Ok)
    } else if dbm >= -75 {
        ("ok", Status::Warn)
    } else {
        ("poor", Status::Bad)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Wake {
    pub last_ms: Option<i64>,
    pub median_24h_ms: Option<i64>,
    pub status: Status,
}

pub fn wake(latest: Option<&Reading>, readings: &[Reading], at: DateTime<Utc>) -> Wake {
    let from = at - Duration::hours(24);
    let in_window = |r: &Reading| r.at > from && r.at <= at;
    let mut values: Vec<i64> = readings
        .iter()
        .filter(|r| in_window(r))
        .filter_map(|r| r.wake_ms)
        .collect();
    if let Some(r) = latest
        && in_window(r)
        && !readings.iter().any(|x| x.at == r.at)
        && let Some(w) = r.wake_ms
    {
        values.push(w);
    }
    values.retain(|w| *w != 0);
    values.sort_unstable();

    let n = values.len();
    let median_24h_ms = match n {
        0 => None,
        _ if n % 2 == 1 => Some(values[n / 2]),
        _ => {
            let (a, b) = (values[n / 2 - 1] as i128, values[n / 2] as i128);
            Some((a + (b - a) / 2) as i64)
        }
    };
    let status = match median_24h_ms {
        None => Status::Unknown,
        Some(m) if m > 10_000 => Status::Warn,
        Some(_) => Status::Ok,
    };
    Wake {
        last_ms: latest.and_then(|r| r.wake_ms).filter(|w| *w != 0),
        median_24h_ms,
        status,
    }
}

pub fn reset_status(reason: &str) -> Status {
    match reason {
        "ESP_RST_BROWNOUT" | "ESP_RST_PANIC" => Status::Bad,
        r if r.ends_with("_WDT") => Status::Bad,
        "ESP_RST_POWERON" | "ESP_RST_SW" | "ESP_RST_EXT" => Status::Ok,
        _ => Status::Warn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap()
    }

    fn r(mins_ago: i64, wake: Option<i64>) -> Reading {
        Reading {
            at: at() - Duration::minutes(mins_ago),
            wake_ms: wake,
            ..Reading::default()
        }
    }

    #[test]
    fn rssi_labels() {
        assert_eq!(rssi_status(-60), ("good", Status::Ok));
        assert_eq!(rssi_status(-67), ("good", Status::Ok));
        assert_eq!(rssi_status(-68), ("ok", Status::Warn));
        assert_eq!(rssi_status(-75), ("ok", Status::Warn));
        assert_eq!(rssi_status(-76), ("poor", Status::Bad));
        assert_eq!(rssi_status(-84), ("poor", Status::Bad));
    }

    #[test]
    fn wake_window_and_median() {
        let rs = vec![
            r(300, Some(4000)),
            r(200, Some(5000)),
            r(100, Some(11000)),
            r(50, Some(0)),
            r(10, Some(12000)),
        ];
        let w = wake(rs.last(), &rs, at());
        assert_eq!(w.median_24h_ms, Some(8000));
        assert_eq!(w.status, Status::Ok);
        assert_eq!(w.last_ms, Some(12000));

        let rs = vec![r(30, Some(11000)), r(20, Some(12000)), r(10, Some(13000))];
        let w = wake(rs.last(), &rs, at());
        assert_eq!(w.median_24h_ms, Some(12000));
        assert_eq!(w.status, Status::Warn);

        // Exactly 24 h before `at` is excluded.
        let rs = vec![r(24 * 60, Some(50_000)), r(10, Some(3000))];
        assert_eq!(wake(rs.last(), &rs, at()).median_24h_ms, Some(3000));

        // Latest known, nothing in the window.
        let old = vec![r(48 * 60, Some(3000))];
        let latest = r(30 * 60, Some(4000));
        let w = wake(Some(&latest), &old, at());
        assert_eq!(w.last_ms, Some(4000));
        assert_eq!(w.median_24h_ms, None);
        assert_eq!(w.status, Status::Unknown);

        let big = i64::MAX / 2 + 1;
        let rs = vec![r(20, Some(big)), r(10, Some(big))];
        assert_eq!(wake(rs.last(), &rs, at()).median_24h_ms, Some(big));
    }

    #[test]
    fn reset_reasons() {
        for b in ["ESP_RST_BROWNOUT", "ESP_RST_PANIC", "ESP_RST_TASK_WDT"] {
            assert_eq!(reset_status(b), Status::Bad);
        }
        assert_eq!(reset_status("ESP_RST_POWERON"), Status::Ok);
        assert_eq!(reset_status("ESP_RST_UNKNOWN"), Status::Warn);
        assert_eq!(reset_status("7"), Status::Warn);
    }
}
