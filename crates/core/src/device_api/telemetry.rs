//! The telemetry a device sends as `/api/display` request headers.

use axum::http::HeaderMap;
use chrono::{DateTime, Utc};

use super::header_str;
use crate::history::Reading;

/// A numeric header, `None` unless it parses and lies in `range`.
fn number<T: std::str::FromStr + PartialOrd>(
    headers: &HeaderMap,
    name: &str,
    range: std::ops::RangeInclusive<T>,
) -> Option<T> {
    header_str(headers, name)?
        .trim()
        .parse()
        .ok()
        .filter(|v| range.contains(v))
}

fn text(headers: &HeaderMap, name: &str) -> Option<String> {
    header_str(headers, name).map(str::to_owned)
}

/// The reading for one request. Anything missing, unparseable or out of range
/// is `None`; strings are kept as sent (the history truncates them).
pub(super) fn reading(mac: &str, at: DateTime<Utc>, headers: &HeaderMap) -> Reading {
    Reading {
        device: mac.to_owned(),
        at,
        // NaN and infinity fail the range check.
        battery_v: number(headers, "Battery-Voltage", 0.0..=10.0),
        rssi: number(headers, "RSSI", -150..=0),
        wifi_band: text(headers, "WiFi-Band"),
        wake_ms: number(headers, "Wake-Time", 0..=86_400_000),
        reset_reason: text(headers, "Reset-Reason"),
        update_source: text(headers, "Update-Source"),
        image_cached: match header_str(headers, "Image-Cached") {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => None,
        },
        refresh_rate: number(headers, "Refresh-Rate", 0..=604_800),
        fw_version: text(headers, "FW-Version"),
        fw_commit: text(headers, "FW-Commit"),
        model: text(headers, "Model"),
        panel_rev: text(headers, "Panel-Rev"),
        ..Reading::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn range_edges() {
        let at = Utc::now();
        let r = reading(
            "M",
            at,
            &headers(&[
                ("Battery-Voltage", "0.0"),
                ("RSSI", "0"),
                ("Wake-Time", "86400000"),
                ("Refresh-Rate", "604800"),
                ("Image-Cached", "false"),
            ]),
        );
        assert_eq!(r.battery_v, Some(0.0));
        assert_eq!(r.rssi, Some(0));
        assert_eq!(r.wake_ms, Some(86_400_000));
        assert_eq!(r.refresh_rate, Some(604_800));
        assert_eq!(r.image_cached, Some(false));

        let r = reading(
            "M",
            at,
            &headers(&[
                ("Battery-Voltage", "10.5"),
                ("RSSI", "-151"),
                ("Wake-Time", "86400001"),
                ("Refresh-Rate", "604801"),
                ("Image-Cached", "yes"),
            ]),
        );
        assert_eq!(
            (
                r.battery_v,
                r.rssi,
                r.wake_ms,
                r.refresh_rate,
                r.image_cached
            ),
            (None, None, None, None, None)
        );
    }
}
