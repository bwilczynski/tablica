//! Per-device metrics state: the latest-only and last-known values behind the
//! scrape-time gauges, merged by each field's own timestamp.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};

use super::labels;
use crate::diagnostics::battery::{self, LatestBattery, Power};
use crate::history::Reading;

/// The most battery samples kept per device.
pub const MAX_SAMPLES: usize = 2_500;

type Sample = (DateTime<Utc>, f64);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeviceState {
    pub last_seen: Option<DateTime<Utc>>,
    pub rssi: Option<(DateTime<Utc>, Option<i64>)>,
    pub last_wake: Option<(DateTime<Utc>, i64)>,
    pub last_image_bytes: Option<(DateTime<Utc>, i64)>,
    pub last_reply: Option<(DateTime<Utc>, i64)>,
    pub last_reset: Option<DateTime<Utc>>,
    pub info: Option<(DateTime<Utc>, Option<&'static str>, Option<String>)>,
    pub latest_battery: Option<LatestBattery>,
    pub samples: VecDeque<Sample>,
}

/// History rows used to warm a device's state at startup.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetricsSeed {
    pub latest: Option<Reading>,
    pub last_wake: Option<Reading>,
    pub last_image: Option<Reading>,
    pub last_reply: Option<Reading>,
    pub last_reset: Option<Reading>,
    /// Non-zero volts in the 7-day window: sorted, unique, at most `MAX_SAMPLES`.
    pub battery: Vec<Sample>,
}

/// True when `at` should replace a value stamped `stored`.
fn newer(at: DateTime<Utc>, stored: Option<DateTime<Utc>>) -> bool {
    stored.is_none_or(|s| at > s)
}

fn apply_latest(st: &mut DeviceState, r: &Reading) {
    if newer(r.at, st.last_seen) {
        st.last_seen = Some(r.at);
    }
    if newer(r.at, st.rssi.map(|x| x.0)) {
        st.rssi = Some((r.at, r.rssi));
    }
    if newer(r.at, st.info.as_ref().map(|x| x.0)) {
        st.info = Some((
            r.at,
            labels::model(r.model.as_deref()),
            labels::fw_version(r.fw_version.as_deref()),
        ));
    }
    if newer(r.at, st.latest_battery.map(|x| x.0)) {
        st.latest_battery = Some((r.at, r.battery_v));
    }
}

fn apply_wake(st: &mut DeviceState, r: &Reading) {
    if let Some(w) = r.wake_ms.filter(|w| *w != 0)
        && newer(r.at, st.last_wake.map(|x| x.0))
    {
        st.last_wake = Some((r.at, w));
    }
}

fn apply_image(st: &mut DeviceState, r: &Reading) {
    if let Some(b) = r.image_bytes
        && newer(r.at, st.last_image_bytes.map(|x| x.0))
    {
        st.last_image_bytes = Some((r.at, b));
    }
}

fn apply_reply(st: &mut DeviceState, r: &Reading) {
    if let Some(n) = r.next_refresh
        && newer(r.at, st.last_reply.map(|x| x.0))
    {
        st.last_reply = Some((r.at, n));
    }
}

fn apply_reset(st: &mut DeviceState, r: &Reading) {
    if labels::counted_reset(r.reset_reason.as_deref()).is_some() && newer(r.at, st.last_reset) {
        st.last_reset = Some(r.at);
    }
}

fn cap(samples: &mut VecDeque<Sample>) {
    while samples.len() > MAX_SAMPLES {
        samples.pop_front();
    }
}

#[derive(Default)]
pub struct DeviceStore {
    devices: Mutex<HashMap<String, DeviceState>>,
}

impl DeviceStore {
    /// Merge one reading. Readings can arrive out of order.
    pub fn record(&self, device: &str, r: &Reading) {
        let mut map = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        let st = map.entry(device.to_owned()).or_default();
        apply_latest(st, r);
        apply_wake(st, r);
        apply_image(st, r);
        apply_reply(st, r);
        apply_reset(st, r);
        if let Some(v) = r.battery_v.filter(|v| *v != 0.0)
            && let Err(i) = st.samples.binary_search_by_key(&r.at, |s| s.0)
        {
            st.samples.insert(i, (r.at, v));
            cap(&mut st.samples);
        }
    }

    /// Merge history rows into the state without overwriting newer live data.
    pub fn seed(&self, device: &str, seed: MetricsSeed) {
        let mut map = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        let st = map.entry(device.to_owned()).or_default();
        if let Some(r) = &seed.latest {
            apply_latest(st, r);
        }
        if let Some(r) = &seed.last_wake {
            apply_wake(st, r);
        }
        if let Some(r) = &seed.last_image {
            apply_image(st, r);
        }
        if let Some(r) = &seed.last_reply {
            apply_reply(st, r);
        }
        if let Some(r) = &seed.last_reset {
            apply_reset(st, r);
        }
        if !seed.battery.is_empty() {
            // Linear merge of two sorted, unique lists; the live sample wins a tie.
            let mut merged = VecDeque::with_capacity(st.samples.len() + seed.battery.len());
            let mut live = std::mem::take(&mut st.samples).into_iter().peekable();
            let mut old = seed.battery.into_iter().peekable();
            loop {
                match (live.peek(), old.peek()) {
                    (Some(l), Some(o)) if l.0 == o.0 => {
                        merged.push_back(*l);
                        live.next();
                        old.next();
                    }
                    (Some(l), Some(o)) if l.0 < o.0 => {
                        merged.push_back(*l);
                        live.next();
                    }
                    (_, Some(o)) => {
                        merged.push_back(*o);
                        old.next();
                    }
                    (Some(l), None) => {
                        merged.push_back(*l);
                        live.next();
                    }
                    (None, None) => break,
                }
            }
            cap(&mut merged);
            st.samples = merged;
        }
    }

    pub fn get(&self, device: &str) -> Option<DeviceState> {
        let map = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        map.get(device).cloned()
    }
}

fn in_window(at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    at > now - Duration::days(7) && at <= now
}

/// Samples within `(now - 7d, now]`.
pub fn window(samples: &VecDeque<Sample>, now: DateTime<Utc>) -> Vec<Sample> {
    samples
        .iter()
        .filter(|s| in_window(s.0, now))
        .copied()
        .collect()
}

/// Battery power from the state, ignoring anything older than 7 days.
pub fn battery_at(state: &DeviceState, now: DateTime<Utc>) -> Power {
    let latest = state.latest_battery.filter(|l| in_window(l.0, now));
    battery::power(latest, &window(&state.samples, now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(n: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + n * 60, 0).unwrap()
    }

    fn rd(n: i64) -> Reading {
        Reading {
            at: t(n),
            ..Default::default()
        }
    }

    #[test]
    fn older_reading_cannot_replace_latest_only() {
        let s = DeviceStore::default();
        s.record(
            "a",
            &Reading {
                rssi: Some(-50),
                ..rd(10)
            },
        );
        s.record(
            "a",
            &Reading {
                rssi: Some(-80),
                model: Some("og".into()),
                ..rd(5)
            },
        );
        let st = s.get("a").unwrap();
        assert_eq!(st.last_seen, Some(t(10)));
        assert_eq!(st.rssi, Some((t(10), Some(-50))));
        assert_eq!(st.info.as_ref().map(|i| i.0), Some(t(10)));
        assert_eq!(st.info.unwrap().1, None);
    }

    #[test]
    fn older_reading_fills_missing_last_known() {
        let s = DeviceStore::default();
        s.record("a", &rd(10));
        s.record(
            "a",
            &Reading {
                wake_ms: Some(3000),
                ..rd(5)
            },
        );
        assert_eq!(s.get("a").unwrap().last_wake, Some((t(5), 3000)));
        s.record(
            "a",
            &Reading {
                wake_ms: Some(9000),
                ..rd(2)
            },
        );
        assert_eq!(s.get("a").unwrap().last_wake, Some((t(5), 3000)));
    }

    #[test]
    fn out_of_order_battery_inserted_sorted() {
        let s = DeviceStore::default();
        for n in [3, 1, 2, 2] {
            s.record(
                "a",
                &Reading {
                    battery_v: Some(3.9),
                    ..rd(n)
                },
            );
        }
        let st = s.get("a").unwrap();
        let ats: Vec<_> = st.samples.iter().map(|x| x.0).collect();
        assert_eq!(ats, vec![t(1), t(2), t(3)]);
    }

    #[test]
    fn rssi_is_latest_only() {
        let s = DeviceStore::default();
        s.record(
            "a",
            &Reading {
                rssi: Some(-60),
                ..rd(1)
            },
        );
        s.record("a", &rd(2));
        assert_eq!(s.get("a").unwrap().rssi, Some((t(2), None)));
    }

    #[test]
    fn last_known_survives_missing_fields() {
        let s = DeviceStore::default();
        s.record(
            "a",
            &Reading {
                wake_ms: Some(1200),
                image_bytes: Some(4096),
                ..rd(1)
            },
        );
        s.record("a", &rd(2));
        let st = s.get("a").unwrap();
        assert_eq!(st.last_wake, Some((t(1), 1200)));
        assert_eq!(st.last_image_bytes, Some((t(1), 4096)));
    }

    #[test]
    fn record_without_reply() {
        let s = DeviceStore::default();
        s.record("a", &rd(1));
        let st = s.get("a").unwrap();
        assert_eq!(st.last_reply, None);
        assert_eq!(st.last_seen, Some(t(1)));
    }

    #[test]
    fn samples_capped_and_sorted() {
        let s = DeviceStore::default();
        for n in (0..2_600).rev() {
            s.record(
                "a",
                &Reading {
                    battery_v: Some(3.8),
                    ..rd(n)
                },
            );
        }
        let st = s.get("a").unwrap();
        assert_eq!(st.samples.len(), MAX_SAMPLES);
        assert_eq!(st.samples.front().unwrap().0, t(100));
        assert_eq!(st.samples.back().unwrap().0, t(2_599));
        assert!(
            st.samples
                .iter()
                .zip(st.samples.iter().skip(1))
                .all(|(a, b)| a.0 < b.0)
        );
    }

    #[test]
    fn seed_never_overwrites_newer_live() {
        let s = DeviceStore::default();
        s.record(
            "a",
            &Reading {
                battery_v: Some(3.9),
                ..rd(10)
            },
        );
        s.seed(
            "a",
            MetricsSeed {
                latest: Some(rd(5)),
                battery: (1..=10).map(|n| (t(n), 3.7)).collect(),
                ..Default::default()
            },
        );
        let st = s.get("a").unwrap();
        assert_eq!(st.last_seen, Some(t(10)));
        let ats: Vec<_> = st.samples.iter().map(|x| x.0).collect();
        assert_eq!(ats, (1..=10).map(t).collect::<Vec<_>>());
        assert_eq!(st.samples.back().unwrap().1, 3.9);
    }

    #[test]
    fn large_seed_merges_with_newer_live() {
        let s = DeviceStore::default();
        for n in 10_000..10_000 + MAX_SAMPLES as i64 {
            s.record(
                "a",
                &Reading {
                    battery_v: Some(3.9),
                    ..rd(n)
                },
            );
        }
        s.seed(
            "a",
            MetricsSeed {
                battery: (0..MAX_SAMPLES as i64).map(|n| (t(n), 3.7)).collect(),
                ..Default::default()
            },
        );
        let st = s.get("a").unwrap();
        assert_eq!(st.samples.len(), MAX_SAMPLES);
        assert!(st.samples.iter().all(|x| x.1 == 3.9));
    }

    #[test]
    fn seed_applies_projected_fields_only() {
        let s = DeviceStore::default();
        s.seed(
            "a",
            MetricsSeed {
                last_wake: Some(Reading {
                    wake_ms: Some(2000),
                    rssi: Some(-70),
                    ..rd(3)
                }),
                ..Default::default()
            },
        );
        let st = s.get("a").unwrap();
        assert_eq!(st.last_wake, Some((t(3), 2000)));
        assert_eq!(st.rssi, None);
        assert_eq!(st.last_seen, None);
    }

    #[test]
    fn offline_device_battery_ages_out() {
        let now = t(100_000);
        let old = now - Duration::days(8);
        let st = DeviceState {
            latest_battery: Some((old, Some(3.9))),
            samples: VecDeque::from([(old, 3.9)]),
            ..Default::default()
        };
        assert_eq!(battery_at(&st, now), Power::NoData);
    }

    #[test]
    fn zero_then_missing() {
        let now = t(100);
        let mut st = DeviceState {
            latest_battery: Some((t(1), Some(0.0))),
            ..Default::default()
        };
        assert_eq!(battery_at(&st, now), Power::NoBattery);
        st.latest_battery = Some((t(2), None));
        st.samples.push_back((t(0), 3.9));
        assert!(matches!(battery_at(&st, now), Power::Battery { .. }));
    }

    #[test]
    fn newer_reading_sets_latest_battery() {
        let s = DeviceStore::default();
        s.record(
            "a",
            &Reading {
                battery_v: Some(3.9),
                ..rd(1)
            },
        );
        assert_eq!(s.get("a").unwrap().latest_battery, Some((t(1), Some(3.9))));
        s.record(
            "a",
            &Reading {
                battery_v: Some(0.0),
                ..rd(2)
            },
        );
        assert_eq!(s.get("a").unwrap().latest_battery, Some((t(2), Some(0.0))));
        s.record("a", &rd(3));
        assert_eq!(s.get("a").unwrap().latest_battery, Some((t(3), None)));
    }

    #[test]
    fn older_reading_leaves_latest_battery() {
        let s = DeviceStore::default();
        s.record(
            "a",
            &Reading {
                battery_v: Some(3.9),
                ..rd(10)
            },
        );
        s.record(
            "a",
            &Reading {
                battery_v: Some(3.5),
                ..rd(5)
            },
        );
        assert_eq!(s.get("a").unwrap().latest_battery, Some((t(10), Some(3.9))));
    }

    #[test]
    fn seed_older_latest_keeps_live_latest_battery() {
        let s = DeviceStore::default();
        s.record(
            "a",
            &Reading {
                battery_v: Some(3.9),
                ..rd(10)
            },
        );
        s.seed(
            "a",
            MetricsSeed {
                latest: Some(Reading {
                    battery_v: Some(3.1),
                    ..rd(5)
                }),
                ..Default::default()
            },
        );
        assert_eq!(s.get("a").unwrap().latest_battery, Some((t(10), Some(3.9))));
    }

    #[test]
    fn older_reading_fills_then_does_not_replace_image_reply_reset() {
        let s = DeviceStore::default();
        s.record("a", &rd(10));
        s.record(
            "a",
            &Reading {
                image_bytes: Some(100),
                next_refresh: Some(60),
                reset_reason: Some("ESP_RST_PANIC".into()),
                ..rd(5)
            },
        );
        let st = s.get("a").unwrap();
        assert_eq!(st.last_image_bytes, Some((t(5), 100)));
        assert_eq!(st.last_reply, Some((t(5), 60)));
        assert_eq!(st.last_reset, Some(t(5)));
        s.record(
            "a",
            &Reading {
                image_bytes: Some(200),
                next_refresh: Some(90),
                reset_reason: Some("ESP_RST_PANIC".into()),
                ..rd(2)
            },
        );
        let st = s.get("a").unwrap();
        assert_eq!(st.last_image_bytes, Some((t(5), 100)));
        assert_eq!(st.last_reply, Some((t(5), 60)));
        assert_eq!(st.last_reset, Some(t(5)));
        s.record(
            "a",
            &Reading {
                image_bytes: Some(300),
                next_refresh: Some(120),
                reset_reason: Some("ESP_RST_PANIC".into()),
                ..rd(8)
            },
        );
        let st = s.get("a").unwrap();
        assert_eq!(st.last_image_bytes, Some((t(8), 300)));
        assert_eq!(st.last_reply, Some((t(8), 120)));
        assert_eq!(st.last_reset, Some(t(8)));
    }

    #[test]
    fn deep_sleep_reset_not_recorded() {
        let s = DeviceStore::default();
        s.record(
            "a",
            &Reading {
                reset_reason: Some(labels::DEEP_SLEEP.into()),
                ..rd(1)
            },
        );
        assert_eq!(s.get("a").unwrap().last_reset, None);
    }

    #[test]
    fn seed_image_and_reply_project_own_fields_only() {
        let s = DeviceStore::default();
        s.seed(
            "a",
            MetricsSeed {
                last_image: Some(Reading {
                    image_bytes: Some(10),
                    next_refresh: Some(30),
                    rssi: Some(-60),
                    ..rd(3)
                }),
                last_reply: Some(Reading {
                    next_refresh: Some(45),
                    image_bytes: Some(20),
                    ..rd(4)
                }),
                ..Default::default()
            },
        );
        let st = s.get("a").unwrap();
        assert_eq!(st.last_image_bytes, Some((t(3), 10)));
        assert_eq!(st.last_reply, Some((t(4), 45)));
        assert_eq!(st.rssi, None);
        assert_eq!(st.last_seen, None);
        assert_eq!(st.last_wake, None);
    }

    #[test]
    fn window_boundaries() {
        let now = t(100_000);
        let samples = VecDeque::from([
            (now - Duration::days(7), 3.0),
            (now - Duration::days(7) + Duration::seconds(1), 3.1),
            (now, 3.2),
            (now + Duration::seconds(1), 3.3),
        ]);
        let w = window(&samples, now);
        assert_eq!(w.iter().map(|x| x.1).collect::<Vec<_>>(), vec![3.1, 3.2]);
    }
}
