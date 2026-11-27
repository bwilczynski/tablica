//! Turning a `/api/log` body into device log rows.

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{DeviceLog, HEADER_MAX, MESSAGE_MAX, RAW_MAX, truncate};

/// One row per entry of the body's `logs` array, all sharing `received_at`.
///
/// A body that is not JSON, or JSON without a `logs` array, becomes one row
/// holding the body as lossy UTF-8 in `raw`, so nothing a device sent is lost.
pub fn parse_log_body(device: &str, received_at: DateTime<Utc>, body: &[u8]) -> Vec<DeviceLog> {
    let raw_row = |raw: String| DeviceLog {
        device: device.to_owned(),
        received_at,
        created_at: None,
        level: None,
        message: None,
        wake_reason: None,
        raw: truncate(raw, RAW_MAX),
    };
    let Ok(Value::Object(mut v)) = serde_json::from_slice::<Value>(body) else {
        return vec![raw_row(String::from_utf8_lossy(body).into_owned())];
    };
    let Some(Value::Array(entries)) = v.remove("logs") else {
        return vec![raw_row(String::from_utf8_lossy(body).into_owned())];
    };
    entries
        .iter()
        .map(|entry| DeviceLog {
            created_at: created_at(&entry["created_at"]),
            level: text(&entry["level"]).map(|s| truncate(s, HEADER_MAX)),
            message: text(&entry["message"]).map(|s| truncate(s, MESSAGE_MAX)),
            wake_reason: text(&entry["wake_reason"]).map(|s| truncate(s, HEADER_MAX)),
            ..raw_row(entry.to_string())
        })
        .collect()
}

/// A string as is; any other non-null value as its JSON text.
fn text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// Unix seconds (a number) or an RFC 3339 string.
fn created_at(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::Number(n) => {
            let secs = n.as_i64().or_else(|| {
                n.as_f64()
                    .filter(|f| f.is_finite() && f.abs() < 1e15)
                    .map(|f| f.floor() as i64)
            })?;
            DateTime::from_timestamp(secs, 0)
        }
        Value::String(s) => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.with_timezone(&Utc)),
        _ => None,
    }
}
