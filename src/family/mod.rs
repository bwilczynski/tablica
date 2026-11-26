//! The family dashboard's data: its caches and how its sources are polled.

pub mod polling;

use std::path::Path;

use chrono::Duration;

use crate::cache::SourceCache;
use crate::sources::calendar::IcsSnapshot;
use crate::sources::librus::timetable::Timetable;
use crate::sources::weather::Weather;

pub const CALENDAR_MAX_AGE: Duration = Duration::hours(2);
pub const TIMETABLE_MAX_AGE: Duration = Duration::hours(26);
pub const WEATHER_MAX_AGE: Duration = Duration::hours(3);

/// All family source caches, one JSON file each under the data directory.
pub struct FamilyCaches {
    pub calendar: SourceCache<IcsSnapshot>,
    pub weather: SourceCache<Weather>,
    /// One cache per kid, in config order.
    pub timetables: Vec<SourceCache<Timetable>>,
}

impl FamilyCaches {
    /// Opens `calendar.json`, `weather.json` and `timetable-kid{N}.json` (N from 1).
    pub fn open(data_dir: &Path, kids: usize) -> FamilyCaches {
        FamilyCaches {
            calendar: SourceCache::open(data_dir.join("calendar.json")),
            weather: SourceCache::open(data_dir.join("weather.json")),
            timetables: (1..=kids)
                .map(|n| SourceCache::open(data_dir.join(format!("timetable-kid{n}.json"))))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};

    fn t(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 25, h, 0, 0).unwrap()
    }

    #[test]
    fn caches_use_expected_file_names() {
        let dir = tempfile::tempdir().unwrap();
        let caches = FamilyCaches::open(dir.path(), 2);
        assert_eq!(caches.timetables.len(), 2);
        let snap = IcsSnapshot {
            text: "ics".into(),
            etag: None,
        };
        caches.calendar.put(snap.clone(), t(8)).unwrap();
        let timetable = Timetable {
            weeks: vec![],
            days: Default::default(),
        };
        caches.timetables[1].put(timetable.clone(), t(9)).unwrap();
        assert!(dir.path().join("calendar.json").exists());
        assert!(dir.path().join("timetable-kid2.json").exists());
        assert!(!dir.path().join("timetable-kid1.json").exists());

        let again = FamilyCaches::open(dir.path(), 2);
        assert_eq!(again.calendar.get().unwrap().data, snap);
        assert!(again.timetables[0].get().is_none());
        assert_eq!(again.timetables[1].get().unwrap().data, timetable);
        assert!(again.weather.get().is_none());
    }

    #[test]
    fn max_ages() {
        assert_eq!(CALENDAR_MAX_AGE, Duration::hours(2));
        assert_eq!(TIMETABLE_MAX_AGE, Duration::hours(26));
        assert_eq!(WEATHER_MAX_AGE, Duration::hours(3));
    }

    fn copy_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cache");
        for entry in std::fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
        }
        dir
    }

    #[test]
    fn pre_split_cache_files_load_unchanged() {
        use crate::dashboards::test_support::{sample_inputs, warsaw};
        let dir = copy_fixture();
        let caches = FamilyCaches::open(dir.path(), 1);
        let expected = sample_inputs(warsaw(2026, 11, 25, 7, 0));
        let fetched_at = Utc.with_ymd_and_hms(2026, 11, 25, 5, 55, 0).unwrap();

        let calendar = caches.calendar.get().unwrap();
        assert_eq!(calendar.fetched_at, fetched_at);
        assert_eq!(calendar.data, expected.calendar.unwrap().data);

        let weather = caches.weather.get().unwrap();
        assert_eq!(weather.fetched_at, fetched_at);
        assert_eq!(weather.data, expected.weather.unwrap().data);

        let timetable = caches.timetables[0].get().unwrap();
        assert_eq!(timetable.fetched_at, fetched_at);
        assert_eq!(timetable.data, expected.timetables[0].clone().unwrap().data);
    }
}
