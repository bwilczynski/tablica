//! Shared fixtures for view-model and render tests.

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::{Europe::Warsaw, Tz};

use super::family::{self, FamilyInputs, FamilyView};
use crate::cache::Stamped;
use crate::config::{FamilyConfig, KidConfig, WeatherConfig};
use crate::sources::calendar::IcsSnapshot;
use crate::sources::librus::timetable::{Lesson, Timetable};
use crate::sources::weather::{Daily, HourlyPrecip, Weather};
use crate::text::{Condition, PrecipKind};

/// Owned source data; borrow it as [`FamilyInputs`] with [`Sample::inputs`].
pub(crate) struct Sample {
    pub cfg: FamilyConfig,
    pub calendar: Option<Stamped<IcsSnapshot>>,
    pub weather: Option<Stamped<Weather>>,
    pub timetables: Vec<Option<Stamped<Timetable>>>,
}

impl Sample {
    pub fn inputs(&self, now: DateTime<Tz>) -> FamilyInputs<'_> {
        FamilyInputs {
            now,
            cfg: &self.cfg,
            calendar: self.calendar.as_ref(),
            weather: self.weather.as_ref(),
            timetables: self.timetables.iter().map(Option::as_ref).collect(),
        }
    }
}

pub(crate) fn warsaw(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Tz> {
    Warsaw.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
}

pub(crate) fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

pub(crate) fn time(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).unwrap()
}

pub(crate) fn config(kids: usize) -> FamilyConfig {
    FamilyConfig {
        switch_at: time(18, 0),
        calendar_days: 7,
        ics_url: "https://example.invalid/cal.ics".into(),
        kids: (1..=kids)
            .map(|n| KidConfig {
                label: format!("Kid {n}"),
                librus_login: format!("login{n}"),
                librus_password: "secret".into(),
            })
            .collect(),
        weather: WeatherConfig {
            latitude: 52.2,
            longitude: 21.0,
        },
    }
}

pub(crate) fn lesson(h: u32, m: u32, subject: &str) -> Lesson {
    let start = time(h, m);
    Lesson {
        start,
        end: start + Duration::minutes(45),
        subject: subject.into(),
        teacher: None,
        room: None,
        cancelled: false,
        flag: None,
    }
}

pub(crate) fn stamped<T>(data: T, at: DateTime<Utc>) -> Stamped<T> {
    Stamped {
        data,
        fetched_at: at,
    }
}

pub(crate) fn weather() -> Weather {
    Weather {
        current_temp_c: 4.4,
        current: Condition::Cloudy,
        today: Daily {
            date: date(2026, 11, 25),
            max_c: 6.0,
            min_c: 1.0,
            condition: Condition::Rain,
        },
        tomorrow: Daily {
            date: date(2026, 11, 26),
            max_c: 3.0,
            min_c: -1.0,
            condition: Condition::Snow,
        },
        hourly: vec![
            HourlyPrecip {
                at: date(2026, 11, 25).and_hms_opt(14, 0, 0).unwrap(),
                probability: 20,
                kind: PrecipKind::Rain,
            },
            HourlyPrecip {
                at: date(2026, 11, 25).and_hms_opt(15, 0, 0).unwrap(),
                probability: 30,
                kind: PrecipKind::Rain,
            },
        ],
    }
}

/// One VEVENT; `start`/`end` are ICS values such as `20261125T163000` (Warsaw)
/// or, when `all_day`, `20261126` (end exclusive).
pub(crate) fn vevent(n: usize, title: &str, start: &str, end: &str, all_day: bool) -> String {
    let (s, e) = if all_day {
        (
            format!("DTSTART;VALUE=DATE:{start}"),
            format!("DTEND;VALUE=DATE:{end}"),
        )
    } else {
        (
            format!("DTSTART;TZID=Europe/Warsaw:{start}"),
            format!("DTEND;TZID=Europe/Warsaw:{end}"),
        )
    };
    format!(
        "BEGIN:VEVENT\r\nUID:ev{n}@test\r\nDTSTAMP:20261101T000000Z\r\n{s}\r\n{e}\r\nSUMMARY:{title}\r\nEND:VEVENT\r\n"
    )
}

/// Adds a `LOCATION` to a [`vevent`].
pub(crate) fn located(vevent: String, location: &str) -> String {
    vevent.replace("END:VEVENT", &format!("LOCATION:{location}\r\nEND:VEVENT"))
}

pub(crate) fn ics(events: &[String]) -> IcsSnapshot {
    IcsSnapshot {
        text: format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\n{}END:VCALENDAR\r\n",
            events.concat()
        ),
        etag: None,
    }
}

fn day(lessons: &[(u32, u32, &str)]) -> Vec<Lesson> {
    lessons.iter().map(|&(h, m, s)| lesson(h, m, s)).collect()
}

/// Realistic data around Wednesday 2026-11-25, fetched five minutes before `now`.
pub(crate) fn sample_inputs(now: DateTime<Tz>) -> Sample {
    let fresh = now.with_timezone(&Utc) - Duration::minutes(5);
    let calendar = ics(&[
        located(
            vevent(
                1,
                "Pływanie (Kid 1)",
                "20261125T163000",
                "20261125T173000",
                false,
            ),
            "Pływalnia Polna",
        ),
        located(
            vevent(2, "Wywiadówka", "20261125T180000", "20261125T193000", false),
            "SP 12\\, sala 21",
        ),
        vevent(3, "Babcia - urodziny", "20261126", "20261127", true),
        vevent(
            4,
            "Pianino (Kid 2)",
            "20261126T171500",
            "20261126T181500",
            false,
        ),
        located(
            vevent(5, "Kino", "20261127T190000", "20261127T210000", false),
            "Kino Muranów\\nul. Gen. Andersa 5\\, Warszawa",
        ),
    ]);

    let mut kid1 = Timetable {
        weeks: vec![],
        days: Default::default(),
    };
    let mut maths = lesson(8, 0, "Matematyka");
    maths.end = time(8, 45);
    let mut history = lesson(9, 50, "Historia");
    history.cancelled = true;
    let mut biology = lesson(11, 50, "Biologia");
    biology.flag = Some("zast.".into());
    let mut english = lesson(13, 40, "Język angielski");
    english.end = time(14, 25);
    kid1.days.insert(
        date(2026, 11, 25),
        vec![
            maths,
            lesson(8, 55, "Język polski"),
            history,
            lesson(10, 55, "WF"),
            biology,
            english,
        ],
    );
    let mut thu = day(&[(8, 55, "Fizyka"), (9, 50, "Chemia"), (11, 45, "Geografia")]);
    thu[2].end = time(13, 30);
    kid1.days.insert(date(2026, 11, 26), thu);
    kid1.days.insert(
        date(2026, 11, 27),
        day(&[(8, 0, "Matematyka"), (8, 55, "Język polski")]),
    );

    let mut kid2 = Timetable {
        weeks: vec![],
        days: Default::default(),
    };
    let mut wed = day(&[
        (8, 0, "Ed. wczesnoszkolna"),
        (8, 55, "Ed. wczesnoszkolna"),
        (9, 50, "Język angielski"),
        (10, 55, "Religia"),
        (11, 50, "WF"),
    ]);
    wed[4].end = time(12, 35);
    kid2.days.insert(date(2026, 11, 25), wed);
    let mut thu = day(&[
        (8, 0, "Ed. wczesnoszkolna"),
        (8, 55, "Ed. wczesnoszkolna"),
        (9, 50, "Muzyka"),
        (10, 55, "Plastyka"),
    ]);
    thu[3].end = time(11, 40);
    kid2.days.insert(date(2026, 11, 26), thu);
    kid2.days
        .insert(date(2026, 11, 27), day(&[(8, 0, "Matematyka")]));

    Sample {
        cfg: config(2),
        calendar: Some(stamped(calendar, fresh)),
        weather: Some(stamped(weather(), fresh)),
        timetables: vec![Some(stamped(kid1, fresh)), Some(stamped(kid2, fresh))],
    }
}

pub(crate) fn sample_view_morning() -> FamilyView {
    let now = warsaw(2026, 11, 25, 7, 0);
    family::build(&sample_inputs(now).inputs(now))
}

pub(crate) fn sample_view_evening() -> FamilyView {
    let now = warsaw(2026, 11, 25, 19, 0);
    family::build(&sample_inputs(now).inputs(now))
}

/// Longest realistic content: long titles and locations, 13 events (overflow), nine lessons
/// per kid with long subjects, cancellations and flags, and a stale footnote.
pub(crate) fn worst_case_inputs(now: DateTime<Tz>) -> Sample {
    let mut sample = sample_inputs(now);
    let long = "Bardzo długi tytuł wydarzenia rodzinnego który nie mieści się w kolumnie";
    let events: Vec<String> = (0..13)
        .map(|i| {
            let d = 25 + i / 4;
            let ev = vevent(
                i as usize,
                long,
                &format!("202611{d}T{:02}0000", 17 + i % 4),
                &format!("202611{d}T{:02}3000", 17 + i % 4),
                false,
            );
            if i % 2 == 0 {
                located(
                    ev,
                    "Centrum Sportu i Rekreacji przy ulicy Bardzo Długiej 123",
                )
            } else {
                ev
            }
        })
        .collect();
    sample.calendar = Some(stamped(
        ics(&events),
        now.with_timezone(&Utc) - Duration::hours(3),
    ));

    let subjects = [
        "Edukacja wczesnoszkolna",
        "Język angielski rozszerzony",
        "Wychowanie fizyczne",
        "Zajęcia z wychowawcą",
        "Technika i informatyka",
        "Matematyka",
        "Religia",
        "Muzyka",
        "Plastyka",
    ];
    let mut tt = Timetable {
        weeks: vec![],
        days: Default::default(),
    };
    for d in [25, 26] {
        let lessons = subjects
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let mut l = lesson(8 + i as u32, 0, s);
                l.cancelled = i == 2;
                l.flag = (i == 4).then(|| "zastępstwo".to_string());
                l
            })
            .collect();
        tt.days.insert(date(2026, 11, d), lessons);
    }
    let fresh = now.with_timezone(&Utc) - Duration::minutes(5);
    sample.timetables = vec![Some(stamped(tt.clone(), fresh)), Some(stamped(tt, fresh))];
    sample
}

pub(crate) fn worst_case_view() -> FamilyView {
    let now = warsaw(2026, 11, 25, 7, 0);
    family::build(&worst_case_inputs(now).inputs(now))
}
