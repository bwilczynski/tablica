//! Polish on-screen strings and formatters. Plain lookup tables: chrono's locale
//! formatting does not produce genitive month names.

use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Timelike};
use chrono_tz::Tz;

use super::{Condition, PrecipKind};

pub const TODAY_HEADING: &str = "DZISIAJ";
pub const TOMORROW_HEADING: &str = "JUTRO";
pub const WEEK_HEADING: &str = "TEN TYDZIEŃ";
pub const NO_EVENTS: &str = "Brak wydarzeń";
pub const ALL_DAY: &str = "cały dzień";
pub const NO_LESSONS: &str = "Brak lekcji";
pub const CALENDAR_UNAVAILABLE: &str = "Kalendarz niedostępny";
pub const TIMETABLE_UNAVAILABLE: &str = "Plan lekcji niedostępny";
pub const WEATHER_UNAVAILABLE: &str = "Pogoda niedostępna";
pub const TOMORROW_PREFIX: &str = "jutro: ";

const WEEKDAYS_LONG: [&str; 7] = [
    "Poniedziałek",
    "Wtorek",
    "Środa",
    "Czwartek",
    "Piątek",
    "Sobota",
    "Niedziela",
];
const WEEKDAYS_SHORT: [&str; 7] = ["pon.", "wt.", "śr.", "czw.", "pt.", "sob.", "niedz."];
const MONTHS_GENITIVE: [&str; 12] = [
    "stycznia",
    "lutego",
    "marca",
    "kwietnia",
    "maja",
    "czerwca",
    "lipca",
    "sierpnia",
    "września",
    "października",
    "listopada",
    "grudnia",
];

fn weekday_idx(d: NaiveDate) -> usize {
    d.weekday().num_days_from_monday() as usize
}

/// "Środa, 25 listopada"
pub fn date_header(d: NaiveDate) -> String {
    format!(
        "{}, {} {}",
        WEEKDAYS_LONG[weekday_idx(d)],
        d.day(),
        MONTHS_GENITIVE[d.month0() as usize]
    )
}

/// "Dziś", "Jutro", otherwise "czw. 26".
pub fn day_label(d: NaiveDate, today: NaiveDate) -> String {
    match relative(d, today) {
        Some(s) => s.to_string(),
        None => format!("{} {}", WEEKDAYS_SHORT[weekday_idx(d)], d.day()),
    }
}

/// "Dziś", "Jutro", otherwise "czw.".
pub fn day_short(d: NaiveDate, today: NaiveDate) -> String {
    match relative(d, today) {
        Some(s) => s.to_string(),
        None => weekday_short(d).to_string(),
    }
}

/// Abbreviated weekday, never relative: "czw.".
pub fn weekday_short(d: NaiveDate) -> &'static str {
    WEEKDAYS_SHORT[weekday_idx(d)]
}

fn relative(d: NaiveDate, today: NaiveDate) -> Option<&'static str> {
    if d == today {
        Some("Dziś")
    } else if today.succ_opt() == Some(d) {
        Some("Jutro")
    } else {
        None
    }
}

/// Open-ended start of an event that runs past today: "od 22:00".
pub fn from(t: NaiveTime) -> String {
    format!("od {}", hm(t))
}

/// Open-ended end of an event that began before today: "do 10:00".
pub fn until(t: NaiveTime) -> String {
    format!("do {}", hm(t))
}

/// Hour without a leading zero: "8:00".
pub fn hm(t: NaiveTime) -> String {
    format!("{}:{:02}", t.hour(), t.minute())
}

/// "8:00–14:25"
pub fn range(a: NaiveTime, b: NaiveTime) -> String {
    format!("{}–{}", hm(a), hm(b))
}

/// Rounded whole degrees; never prints "-0".
pub fn temp(c: f32) -> String {
    let r = c.round() as i32;
    format!("{r}°")
}

pub fn high_low(max: f32, min: f32) -> String {
    format!("maks. {} min. {}", temp(max), temp(min))
}

pub fn condition(c: Condition) -> &'static str {
    match c {
        Condition::Clear => "Bezchmurnie",
        Condition::PartlyCloudy => "Częściowe zachmurzenie",
        Condition::Cloudy => "Pochmurno",
        Condition::Fog => "Mgła",
        Condition::Rain => "Deszcz",
        Condition::Snow => "Śnieg",
        Condition::Storm => "Burza",
    }
}

/// "deszcz 30% od 15:00"
pub fn precip_note(kind: PrecipKind, prob: u8, at: NaiveTime) -> String {
    let word = match kind {
        PrecipKind::Rain => "deszcz",
        PrecipKind::Snow => "śnieg",
    };
    format!("{word} {prob}% od {}", hm(at))
}

/// "+3 więcej"
pub fn more(n: usize) -> String {
    format!("+{n} więcej")
}

/// "stan na 7:30" for today, otherwise "stan na pon. 7:30".
pub fn stale(at: DateTime<Tz>, today: NaiveDate) -> String {
    let date = at.date_naive();
    let time = hm(at.time());
    if date == today {
        format!("stan na {time}")
    } else {
        format!("stan na {} {time}", WEEKDAYS_SHORT[weekday_idx(date)])
    }
}

/// Short on-screen tag for a Librus lesson note, or `None` when nothing needs
/// showing: cancelled lessons are already struck through.
pub fn lesson_flag(raw: &str) -> Option<String> {
    let parts: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.iter().any(|p| p.to_lowercase() == "zastępstwo") {
        return Some("zast.".to_owned());
    }
    let first = parts
        .iter()
        .find(|p| p.to_lowercase() != "odwołane")?
        .split_whitespace()
        .next()?;
    if first.chars().count() > 8 {
        Some(format!("{}.", first.chars().take(8).collect::<String>()))
    } else {
        Some(first.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lesson_flags_are_short() {
        assert_eq!(lesson_flag("zastępstwo").as_deref(), Some("zast."));
        assert_eq!(lesson_flag("Zastępstwo").as_deref(), Some("zast."));
        assert_eq!(
            lesson_flag("odwołane, zastępstwo").as_deref(),
            Some("zast.")
        );
        assert_eq!(lesson_flag("odwołane"), None);
        assert_eq!(lesson_flag("Odwołane"), None);
        assert_eq!(lesson_flag("  "), None);
        assert_eq!(
            lesson_flag("przeniesione na wtorek").as_deref(),
            Some("przenies.")
        );
        assert_eq!(lesson_flag("zmiana").as_deref(), Some("zmiana"));
        assert_eq!(lesson_flag("12345678").as_deref(), Some("12345678"));
    }
    use crate::text::{Condition, PrecipKind};
    use chrono::{NaiveDate, NaiveTime, TimeZone};
    use chrono_tz::Europe::Warsaw;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }
    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn date_header_uses_genitive() {
        assert_eq!(date_header(d(2026, 11, 25)), "Środa, 25 listopada");
        let months = [
            "stycznia",
            "lutego",
            "marca",
            "kwietnia",
            "maja",
            "czerwca",
            "lipca",
            "sierpnia",
            "września",
            "października",
            "listopada",
            "grudnia",
        ];
        for (i, m) in months.iter().enumerate() {
            let s = date_header(d(2026, i as u32 + 1, 1));
            assert!(s.ends_with(&format!("1 {m}")), "{s}");
        }
    }

    #[test]
    fn day_labels() {
        let today = d(2026, 11, 24);
        assert_eq!(day_label(today, today), "Dziś");
        assert_eq!(day_label(d(2026, 11, 25), today), "Jutro");
        assert_eq!(day_label(d(2026, 11, 26), today), "czw. 26");
        assert_eq!(day_short(d(2026, 11, 26), today), "czw.");
        assert_eq!(day_short(today, today), "Dziś");
        assert_eq!(day_short(d(2026, 11, 25), today), "Jutro");
        assert_eq!(weekday_short(d(2026, 11, 25)), "śr.");
        let names = ["pon.", "wt.", "śr.", "czw.", "pt.", "sob.", "niedz."];
        for (i, n) in names.iter().enumerate() {
            assert_eq!(day_short(d(2026, 12, 7 + i as u32), today), *n);
        }
    }

    #[test]
    fn times() {
        assert_eq!(hm(t(8, 0)), "8:00");
        assert_eq!(range(t(8, 0), t(14, 25)), "8:00–14:25");
    }

    #[test]
    fn temps() {
        assert_eq!(temp(4.4), "4°");
        assert_eq!(temp(-0.4), "0°");
        assert_eq!(temp(-1.6), "-2°");
        assert_eq!(high_low(6.0, 1.0), "maks. 6° min. 1°");
    }

    #[test]
    fn conditions() {
        assert_eq!(condition(Condition::Clear), "Bezchmurnie");
        assert_eq!(condition(Condition::PartlyCloudy), "Częściowe zachmurzenie");
        assert_eq!(condition(Condition::Cloudy), "Pochmurno");
        assert_eq!(condition(Condition::Fog), "Mgła");
        assert_eq!(condition(Condition::Rain), "Deszcz");
        assert_eq!(condition(Condition::Snow), "Śnieg");
        assert_eq!(condition(Condition::Storm), "Burza");
    }

    #[test]
    fn notes() {
        assert_eq!(
            precip_note(PrecipKind::Rain, 30, t(15, 0)),
            "deszcz 30% od 15:00"
        );
        assert_eq!(
            precip_note(PrecipKind::Snow, 40, t(9, 0)),
            "śnieg 40% od 9:00"
        );
        assert_eq!(more(3), "+3 więcej");
        assert_eq!(from(NaiveTime::from_hms_opt(22, 0, 0).unwrap()), "od 22:00");
        assert_eq!(until(NaiveTime::from_hms_opt(9, 5, 0).unwrap()), "do 9:05");
        let today = d(2026, 11, 25);
        let mon = Warsaw.with_ymd_and_hms(2026, 11, 23, 7, 30, 0).unwrap();
        assert_eq!(stale(mon, today), "stan na pon. 7:30");
        let same = Warsaw.with_ymd_and_hms(2026, 11, 25, 7, 30, 0).unwrap();
        assert_eq!(stale(same, today), "stan na 7:30");
    }

    #[test]
    fn consts() {
        assert_eq!(TODAY_HEADING, "DZISIAJ");
        assert_eq!(TOMORROW_HEADING, "JUTRO");
        assert_eq!(WEEK_HEADING, "TEN TYDZIEŃ");
        assert_eq!(NO_EVENTS, "Brak wydarzeń");
        assert_eq!(ALL_DAY, "cały dzień");
        assert_eq!(NO_LESSONS, "Brak lekcji");
        assert_eq!(CALENDAR_UNAVAILABLE, "Kalendarz niedostępny");
        assert_eq!(TIMETABLE_UNAVAILABLE, "Plan lekcji niedostępny");
        assert_eq!(WEATHER_UNAVAILABLE, "Pogoda niedostępna");
        assert_eq!(TOMORROW_PREFIX, "jutro: ");
    }
}
