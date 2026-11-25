//! JSON-backed cache holding the last good data of one source.

use std::io;
use std::path::PathBuf;
use std::sync::RwLock;

use chrono::{DateTime, Duration, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const CALENDAR_MAX_AGE: Duration = Duration::hours(2);
pub const TIMETABLE_MAX_AGE: Duration = Duration::hours(26);
pub const WEATHER_MAX_AGE: Duration = Duration::hours(3);

/// Data together with the moment it was fetched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stamped<T> {
    pub data: T,
    pub fetched_at: DateTime<Utc>,
}

struct Inner<T> {
    stamped: Option<Stamped<T>>,
    error: Option<(DateTime<Utc>, String)>,
}

/// Last good data of one source, mirrored to a JSON file.
///
/// Errors are kept in memory only.
pub struct SourceCache<T> {
    path: PathBuf,
    inner: RwLock<Inner<T>>,
}

impl<T: Serialize + DeserializeOwned + Clone> SourceCache<T> {
    /// Loads the file if present; a corrupt or unreadable file is logged and ignored.
    pub fn open(path: PathBuf) -> Self {
        let stamped = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(s) => Some(s),
                Err(e) => {
                    eprintln!("ignoring corrupt cache {}: {e}", path.display());
                    None
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                eprintln!("cannot read cache {}: {e}", path.display());
                None
            }
        };
        Self {
            path,
            inner: RwLock::new(Inner {
                stamped,
                error: None,
            }),
        }
    }

    pub fn get(&self) -> Option<Stamped<T>> {
        self.read().stamped.clone()
    }

    /// Stores the data and writes it atomically (temp file, then rename).
    ///
    /// The in-memory value is updated even when the write fails, and a
    /// successful put clears the recorded error.
    pub fn put(&self, data: T, at: DateTime<Utc>) -> io::Result<()> {
        let stamped = Stamped {
            data,
            fetched_at: at,
        };
        let result = self.write_file(&stamped);
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        inner.stamped = Some(stamped);
        inner.error = None;
        result
    }

    pub fn record_error(&self, msg: String, at: DateTime<Utc>) {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        inner.error = Some((at, msg));
    }

    pub fn last_error(&self) -> Option<(DateTime<Utc>, String)> {
        self.read().error.clone()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner<T>> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_file(&self, stamped: &Stamped<T>) -> io::Result<()> {
        let bytes = serde_json::to_vec(stamped).map_err(io::Error::other)?;
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let written = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, &self.path));
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written
    }
}

/// True when the data is strictly older than `max_age`.
pub fn is_stale(fetched_at: DateTime<Utc>, now: DateTime<Utc>, max_age: Duration) -> bool {
    now - fetched_at > max_age
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 25, h, 0, 0).unwrap()
    }

    #[test]
    fn put_then_reopen_returns_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let cache = SourceCache::<Vec<String>>::open(path.clone());
        assert!(cache.get().is_none());
        cache.put(vec!["a".into()], t(8)).unwrap();
        let reopened = SourceCache::<Vec<String>>::open(path);
        let got = reopened.get().unwrap();
        assert_eq!(got.data, vec!["a".to_string()]);
        assert_eq!(got.fetched_at, t(8));
    }

    #[test]
    fn corrupt_file_yields_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, "{ not json").unwrap();
        let cache = SourceCache::<Vec<String>>::open(path);
        assert!(cache.get().is_none());
    }

    #[test]
    fn put_does_not_leave_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SourceCache::<u32>::open(dir.path().join("c.json"));
        cache.put(1, t(8)).unwrap();
        cache.put(2, t(9)).unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("c.json")]);
    }

    #[test]
    fn stale_boundaries() {
        let max = Duration::hours(2);
        assert!(!is_stale(t(8), t(10), max));
        assert!(is_stale(t(8), t(10) + Duration::seconds(1), max));
    }

    #[test]
    fn errors_are_memory_only_and_cleared_by_put() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let cache = SourceCache::<u32>::open(path.clone());
        cache.record_error("boom".into(), t(8));
        assert_eq!(cache.last_error(), Some((t(8), "boom".to_string())));
        cache.put(1, t(9)).unwrap();
        assert!(cache.last_error().is_none());
        cache.record_error("again".into(), t(10));
        assert!(SourceCache::<u32>::open(path).last_error().is_none());
    }

    #[test]
    fn cache_is_send_sync() {
        fn assert_ss<X: Send + Sync>() {}
        assert_ss::<SourceCache<Vec<String>>>();
    }

    #[test]
    fn max_ages() {
        assert_eq!(CALENDAR_MAX_AGE, Duration::hours(2));
        assert_eq!(TIMETABLE_MAX_AGE, Duration::hours(26));
        assert_eq!(WEATHER_MAX_AGE, Duration::hours(3));
    }
}
