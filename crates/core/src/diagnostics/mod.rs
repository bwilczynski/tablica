//! The diagnostics contract: what each dashboard contributes to a device's
//! diagnostics screen.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::cache::{SourceCache, is_stale};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Bad,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    pub label: String,
    pub value: String,
    pub status: Status,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Section {
    pub title: String,
    pub rows: Vec<Row>,
}

/// A coarse human age: "just now", "N min", "N h" or "N d".
pub fn format_age(age: Duration) -> String {
    if age < Duration::seconds(60) {
        "just now".to_owned()
    } else if age < Duration::minutes(60) {
        format!("{} min", age.num_minutes())
    } else if age < Duration::hours(48) {
        format!("{} h", age.num_hours())
    } else {
        format!("{} d", age.num_days())
    }
}

/// One row for a source cache: its age and freshness, with the last refresh
/// error as the note.
pub fn source_row<T: Serialize + DeserializeOwned + Clone>(
    label: &str,
    cache: &SourceCache<T>,
    now: DateTime<Utc>,
    max_age: Duration,
) -> Row {
    let (value, status) = match cache.get() {
        None => ("no data".to_owned(), Status::Bad),
        Some(stamped) => {
            let status = if is_stale(stamped.fetched_at, now, max_age) {
                Status::Warn
            } else {
                Status::Ok
            };
            (format_age(now - stamped.fetched_at), status)
        }
    };
    Row {
        label: label.to_owned(),
        value,
        status,
        note: cache.last_error().map(|(_, msg)| msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn format_age_buckets() {
        assert_eq!(format_age(Duration::seconds(30)), "just now");
        assert_eq!(format_age(Duration::minutes(12)), "12 min");
        assert_eq!(format_age(Duration::hours(3)), "3 h");
        assert_eq!(format_age(Duration::hours(50)), "2 d");
        assert_eq!(format_age(Duration::minutes(-5)), "just now");
    }

    #[test]
    fn source_row_statuses() {
        let dir = tempfile::tempdir().unwrap();
        let cache: SourceCache<String> = SourceCache::open(dir.path().join("c.json"));
        let now = Utc.with_ymd_and_hms(2026, 11, 25, 12, 0, 0).unwrap();
        let max = Duration::hours(2);

        let row = source_row("src", &cache, now, max);
        assert_eq!((row.value.as_str(), row.status), ("no data", Status::Bad));
        assert_eq!(row.label, "src");
        assert_eq!(row.note, None);

        cache.put("x".into(), now - Duration::hours(1)).unwrap();
        let row = source_row("src", &cache, now, max);
        assert_eq!((row.value.as_str(), row.status), ("1 h", Status::Ok));

        cache.put("x".into(), now - Duration::hours(3)).unwrap();
        let row = source_row("src", &cache, now, max);
        assert_eq!((row.value.as_str(), row.status), ("3 h", Status::Warn));

        cache.record_error("login failed".into(), now);
        let row = source_row("src", &cache, now, max);
        assert_eq!(row.note.as_deref(), Some("login failed"));
    }
}
