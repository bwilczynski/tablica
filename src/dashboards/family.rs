//! Family dashboard view model: final display strings, no layout.

use std::ops::Bound;

use chrono::{DateTime, Duration, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use serde::Serialize;

use crate::cache::{Stamped, is_stale};
use crate::family::config::FamilyConfig;
use crate::family::{CALENDAR_MAX_AGE, TIMETABLE_MAX_AGE, WEATHER_MAX_AGE};
use crate::sources::calendar::{self, CalendarEvent, EventWhen, IcsSnapshot};
use crate::sources::librus::timetable::{Lesson, Timetable};
use crate::sources::weather::Weather;
use crate::text::{Condition, pl};

// Calendar heights in px, mirroring `templates/family.html`. Rows that don't
// fit collapse into a "+N więcej" line.
/// Column height under the first heading: 416 − 20 padding − 25.25 `.h4`, rounded down.
const CALENDAR_PX: u32 = 370;
/// `.small` stale note: 12px × 1.25.
const NOTE_PX: u32 = 15;
/// `.more`: 19px line plus 6px top margin.
const MORE_PX: u32 = 25;
/// `.h4.sec`: 25.25px heading plus 12px top margin, rounded up.
const SECTION_PX: u32 = 38;
/// `.cday`: 22px band plus 6px top margin.
const DAY_PX: u32 = 28;
/// `.ev` and `.none`: 19px line plus 4px top margin.
const EVENT_PX: u32 = 23;
/// `.loc`: 16px second line.
const LOCATION_PX: u32 = 16;

/// Minimum precipitation probability worth mentioning, in percent.
const PRECIP_THRESHOLD: u8 = 30;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FamilyView {
    pub header: Header,
    pub calendar: CalendarCol,
    pub kids: Vec<KidCol>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Header {
    pub date: String,
    pub temp: Option<String>,
    pub condition: Option<String>,
    /// One of `clear|partly-cloudy|cloudy|fog|rain|snow|storm`.
    pub icon: Option<&'static str>,
    pub detail: Option<String>,
    /// Stale or unavailable text.
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CalendarCol {
    /// "DZISIAJ", or "JUTRO" from `switch_at` on.
    pub heading: &'static str,
    /// The featured day's events in detail: time range, title and location.
    pub featured: Vec<EventLine>,
    /// Shown instead of `featured` when nothing is left that day.
    pub featured_empty: Option<&'static str>,
    /// The rest of the window, one line per event. `None` when there is no
    /// calendar or no room for it.
    pub week: Option<WeekSection>,
    pub more: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WeekSection {
    pub heading: &'static str,
    pub days: Vec<CalDay>,
    /// Shown instead of `days` when nothing else is planned.
    pub empty: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CalDay {
    #[serde(skip)]
    pub date: NaiveDate,
    pub label: String,
    pub events: Vec<EventLine>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventLine {
    pub time: String,
    pub title: String,
    pub location: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KidCol {
    pub label: String,
    pub featured: Option<Featured>,
    pub summary: Option<Summary>,
    pub empty: Option<&'static str>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Featured {
    pub title: String,
    pub lessons: Vec<LessonLine>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LessonLine {
    pub time: String,
    pub subject: String,
    pub cancelled: bool,
    pub flag: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub title: String,
    pub subjects: Option<String>,
}

pub struct FamilyInputs<'a> {
    pub now: DateTime<Tz>,
    pub cfg: &'a FamilyConfig,
    pub calendar: Option<&'a Stamped<IcsSnapshot>>,
    pub weather: Option<&'a Stamped<Weather>>,
    /// One entry per kid, in config order.
    pub timetables: Vec<Option<&'a Stamped<Timetable>>>,
}

pub fn build(inputs: &FamilyInputs) -> FamilyView {
    let now = inputs.now;
    let today = now.date_naive();
    FamilyView {
        header: build_header(inputs, today),
        calendar: build_calendar(inputs, today),
        kids: inputs
            .cfg
            .kids
            .iter()
            .enumerate()
            .map(|(i, kid)| {
                let tt = inputs.timetables.get(i).copied().flatten();
                build_kid(&kid.label.to_uppercase(), tt, inputs)
            })
            .collect(),
    }
}

/// Stale footnote for a source, if it is older than `max_age`.
fn stale_note<T>(stamped: &Stamped<T>, max_age: Duration, now: DateTime<Tz>) -> Option<String> {
    let now_utc = now.with_timezone(&Utc);
    is_stale(stamped.fetched_at, now_utc, max_age).then(|| {
        pl::stale(
            stamped.fetched_at.with_timezone(&now.timezone()),
            now.date_naive(),
        )
    })
}

fn icon(c: Condition) -> &'static str {
    match c {
        Condition::Clear => "clear",
        Condition::PartlyCloudy => "partly-cloudy",
        Condition::Cloudy => "cloudy",
        Condition::Fog => "fog",
        Condition::Rain => "rain",
        Condition::Snow => "snow",
        Condition::Storm => "storm",
    }
}

fn build_header(inputs: &FamilyInputs, today: NaiveDate) -> Header {
    let mut header = Header {
        date: pl::date_header(today),
        temp: None,
        condition: None,
        icon: None,
        detail: None,
        note: None,
    };
    let Some(stamped) = inputs.weather else {
        header.note = Some(pl::WEATHER_UNAVAILABLE.to_string());
        return header;
    };
    let w = &stamped.data;
    header.temp = Some(pl::temp(w.current_temp_c));
    header.condition = Some(pl::condition(w.current).to_string());
    header.icon = Some(icon(w.current));
    header.note = stale_note(stamped, WEATHER_MAX_AGE, inputs.now);

    let after_switch = inputs.now.time() >= inputs.cfg.switch_at;
    header.detail = if after_switch {
        today.succ_opt().and_then(|d| w.day(d)).map(|t| {
            format!(
                "{}{} · {}",
                pl::TOMORROW_PREFIX,
                pl::high_low(t.max_c, t.min_c),
                pl::condition(t.condition).to_lowercase()
            )
        })
    } else {
        let mut parts: Vec<String> = w
            .day(today)
            .map(|d| pl::high_low(d.max_c, d.min_c))
            .into_iter()
            .collect();
        let hour = inputs.now.hour();
        let first = w.hourly.iter().find(|h| {
            h.at.date() == today && h.at.hour() >= hour && h.probability >= PRECIP_THRESHOLD
        });
        if let Some(h) = first {
            parts.push(pl::precip_note(h.kind, h.probability, h.at.time()));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    };
    header
}

fn build_calendar(inputs: &FamilyInputs, today: NaiveDate) -> CalendarCol {
    let after_switch = inputs.now.time() >= inputs.cfg.switch_at;
    let mut col = CalendarCol {
        heading: pl::TODAY_HEADING,
        featured: vec![],
        featured_empty: None,
        week: None,
        more: None,
        note: None,
    };
    let Some(stamped) = inputs.calendar else {
        col.note = Some(pl::CALENDAR_UNAVAILABLE.to_string());
        return col;
    };
    let last_day = today + Duration::days(i64::from(inputs.cfg.calendar_days.max(1)) - 1);
    let tz = inputs.now.timezone();
    // Always expand through tomorrow: it may become the featured day even when
    // the configured window ends today.
    let tomorrow = today.succ_opt().unwrap_or(today);
    let events = match calendar::events_in(&stamped.data.text, tz, today, last_day.max(tomorrow)) {
        Ok(events) => events,
        Err(e) => {
            tracing::warn!("cannot expand calendar: {e}");
            col.note = Some(pl::CALENDAR_UNAVAILABLE.to_string());
            return col;
        }
    };
    col.note = stale_note(stamped, CALENDAR_MAX_AGE, inputs.now);

    // Past `switch_at`, tomorrow takes over once nothing is left today.
    let featured = if after_switch && !events.iter().any(|ev| ends_later_today(ev, inputs.now)) {
        col.heading = pl::TOMORROW_HEADING;
        tomorrow
    } else {
        today
    };
    let last_day = last_day.max(featured);

    let mut entries: Vec<Entry> = vec![];
    for ev in &events {
        collect_entries(ev, inputs.now, today, featured, last_day, &mut entries);
    }
    entries.sort_by_key(|e| e.key);
    let (featured_entries, week_entries): (Vec<&Entry>, Vec<&Entry>) =
        entries.iter().partition(|e| e.key.0 == featured);

    // Rows top to bottom with their heights. The week heading rides on the
    // first week row so it is never shown alone.
    let mut rows: Vec<(Row, u32)> = vec![];
    if featured_entries.is_empty() {
        rows.push((Row::FeaturedEmpty, EVENT_PX));
    }
    for e in featured_entries {
        rows.push((
            Row::Featured(e),
            EVENT_PX + u32::from(e.location.is_some()) * LOCATION_PX,
        ));
    }
    for (i, e) in week_entries.iter().enumerate() {
        let new_day = i == 0 || week_entries[i - 1].key.0 != e.key.0;
        let heading = if i == 0 { SECTION_PX } else { 0 };
        rows.push((
            Row::Week(e),
            heading + u32::from(new_day) * DAY_PX + EVENT_PX,
        ));
    }

    let room = CALENDAR_PX - if col.note.is_some() { NOTE_PX } else { 0 };
    let total: u32 = rows.iter().map(|r| r.1).sum();
    // An empty week only says so when there is room left after the events.
    if week_entries.is_empty() && total + SECTION_PX + EVENT_PX <= room {
        col.week = Some(WeekSection {
            heading: pl::WEEK_HEADING,
            days: vec![],
            empty: Some(pl::NO_EVENTS),
        });
    }
    let shown = if total <= room {
        rows.len()
    } else {
        let budget = room - MORE_PX;
        rows.iter()
            .scan(0, |used, r| {
                *used += r.1;
                Some(*used)
            })
            .take_while(|used| *used <= budget)
            .count()
    };

    let hidden = rows[shown..]
        .iter()
        .filter(|r| !matches!(r.0, Row::FeaturedEmpty))
        .count();
    for (row, _) in rows.into_iter().take(shown) {
        match row {
            Row::FeaturedEmpty => col.featured_empty = Some(pl::NO_EVENTS),
            Row::Featured(e) => col.featured.push(EventLine {
                time: event_time(e, featured),
                title: e.title.clone(),
                location: e.location.clone(),
            }),
            Row::Week(e) => {
                let week = col.week.get_or_insert_with(|| WeekSection {
                    heading: pl::WEEK_HEADING,
                    days: vec![],
                    empty: None,
                });
                let date = e.key.0;
                if week.days.last().is_none_or(|d| d.date != date) {
                    week.days.push(CalDay {
                        date,
                        label: pl::day_label(date, today),
                        events: vec![],
                    });
                }
                if let Some(day) = week.days.last_mut() {
                    day.events.push(EventLine {
                        time: event_time(e, date),
                        title: e.title.clone(),
                        location: None,
                    });
                }
            }
        }
    }
    if hidden > 0 {
        col.more = Some(pl::more(hidden));
    }
    col
}

enum Row<'a> {
    FeaturedEmpty,
    Featured(&'a Entry),
    Week(&'a Entry),
}

/// (date, timed?, start): all-day events lead each day.
type SortKey = (NaiveDate, bool, DateTime<Tz>);

/// One calendar event placed on one day of the window.
struct Entry {
    key: SortKey,
    span: Span,
    title: String,
    location: Option<String>,
}

#[derive(Clone, Copy)]
enum Span {
    AllDay,
    Timed {
        start: DateTime<Tz>,
        end: DateTime<Tz>,
    },
}

/// Time of an event as listed under `day`: "16:30–17:30", or open-ended for
/// events that cross midnight.
fn event_time(e: &Entry, day: NaiveDate) -> String {
    let Span::Timed { start, end } = e.span else {
        return pl::ALL_DAY.to_string();
    };
    if end <= start {
        return pl::hm(start.time());
    }
    match (
        start.date_naive() < day,
        last_moment(start, end).date_naive() > day,
    ) {
        (true, true) => pl::ALL_DAY.to_string(),
        (true, false) => pl::until(end.time()),
        (false, true) => pl::from(start.time()),
        (false, false) => pl::range(start.time(), end.time()),
    }
}

/// A timed event that has not ended yet and ends today. All-day events and
/// events running past midnight do not hold off the switch to tomorrow: the
/// former would hold it all evening, the latter are listed under tomorrow.
fn ends_later_today(ev: &CalendarEvent, now: DateTime<Tz>) -> bool {
    let EventWhen::Timed { start, end } = &ev.when else {
        return false;
    };
    !ev.title.trim().is_empty()
        && last_moment(*start, *end) >= now
        && last_moment(*start, *end).date_naive() == now.date_naive()
}

/// The last instant an event occupies: an event ending exactly at midnight
/// does not reach the next day.
fn last_moment(start: DateTime<Tz>, end: DateTime<Tz>) -> DateTime<Tz> {
    if end > start {
        end - Duration::nanoseconds(1)
    } else {
        start
    }
}

/// Places each event on one day: all-day events on every day they cover, timed
/// ones on the featured day when they reach it, otherwise on their start day
/// (today for events already running).
fn collect_entries(
    ev: &CalendarEvent,
    now: DateTime<Tz>,
    today: NaiveDate,
    featured: NaiveDate,
    last_day: NaiveDate,
    out: &mut Vec<Entry>,
) {
    let title = ev.title.trim();
    if title.is_empty() {
        return;
    }
    match &ev.when {
        EventWhen::AllDay { first, last } => {
            let mut d = (*first).max(today);
            while d <= (*last).min(last_day) {
                out.push(Entry {
                    key: (d, false, now),
                    span: Span::AllDay,
                    title: title.to_string(),
                    location: ev.location.clone(),
                });
                let Some(next) = d.succ_opt() else { break };
                d = next;
            }
        }
        EventWhen::Timed { start, end } => {
            let ended = if end > start {
                *end <= now
            } else {
                *start < now
            };
            let reaches_featured = start.date_naive() <= featured
                && last_moment(*start, *end).date_naive() >= featured;
            let date = if reaches_featured {
                featured
            } else {
                start.date_naive().max(today)
            };
            if ended || date > last_day {
                return;
            }
            out.push(Entry {
                key: (date, true, *start),
                span: Span::Timed {
                    start: *start,
                    end: *end,
                },
                title: title.to_string(),
                location: ev.location.clone(),
            });
        }
    }
}

fn day_lessons(tt: &Timetable, d: NaiveDate) -> Option<&Vec<Lesson>> {
    tt.days.get(&d).filter(|l| !l.is_empty())
}

/// The first date strictly after `after` with at least one lesson.
pub fn next_school_day(tt: &Timetable, after: NaiveDate) -> Option<NaiveDate> {
    tt.days
        .range((Bound::Excluded(after), Bound::Unbounded))
        .find(|(_, lessons)| !lessons.is_empty())
        .map(|(d, _)| *d)
}

fn build_kid(label: &str, tt: Option<&Stamped<Timetable>>, inputs: &FamilyInputs) -> KidCol {
    let mut kid = KidCol {
        label: label.to_string(),
        featured: None,
        summary: None,
        empty: None,
        note: None,
    };
    let Some(stamped) = tt else {
        kid.note = Some(pl::TIMETABLE_UNAVAILABLE.to_string());
        return kid;
    };
    kid.note = stale_note(stamped, TIMETABLE_MAX_AGE, inputs.now);
    let tt = &stamped.data;
    let today = inputs.now.date_naive();
    let after_switch = inputs.now.time() >= inputs.cfg.switch_at;

    let (featured_day, summary) = if after_switch {
        let summary = day_lessons(tt, today).map(|l| Summary {
            title: day_title(&pl::day_short(today, today), l),
            subjects: None,
        });
        (next_school_day(tt, today), summary)
    } else {
        let featured = if day_lessons(tt, today).is_some() {
            Some(today)
        } else {
            next_school_day(tt, today)
        };
        let summary = featured
            .and_then(|f| next_school_day(tt, f))
            .and_then(|d| day_lessons(tt, d).map(|l| (d, l)))
            .map(|(d, l)| Summary {
                title: day_title(pl::weekday_short(d), l),
                subjects: subject_list(l),
            });
        (featured, summary)
    };

    match featured_day.and_then(|d| day_lessons(tt, d).map(|l| (d, l))) {
        Some((d, lessons)) => {
            kid.featured = Some(Featured {
                title: day_title(&pl::day_short(d, today), lessons),
                lessons: lessons
                    .iter()
                    .map(|l| LessonLine {
                        time: pl::hm(l.start),
                        subject: l.subject.clone(),
                        cancelled: l.cancelled,
                        flag: l.flag.as_deref().and_then(pl::lesson_flag),
                    })
                    .collect(),
            });
        }
        None => kid.empty = Some(pl::NO_LESSONS),
    }
    kid.summary = summary;
    kid
}

/// "<day> · <first start>–<last end>", cancelled lessons included.
fn day_title(day: &str, lessons: &[Lesson]) -> String {
    let start = lessons.iter().map(|l| l.start).min();
    let end = lessons.iter().map(|l| l.end).max();
    match (start, end) {
        (Some(s), Some(e)) => format!("{day} · {}", pl::range(s, e)),
        _ => day.to_string(),
    }
}

/// Subjects of the lessons that take place, with consecutive repeats as "X ×2".
fn subject_list(lessons: &[Lesson]) -> Option<String> {
    let mut runs: Vec<(&str, usize)> = vec![];
    for l in lessons.iter().filter(|l| !l.cancelled) {
        match runs.last_mut() {
            Some((s, n)) if *s == l.subject => *n += 1,
            _ => runs.push((&l.subject, 1)),
        }
    }
    if runs.is_empty() {
        return None;
    }
    let parts: Vec<String> = runs
        .into_iter()
        .map(|(s, n)| {
            if n > 1 {
                format!("{s} ×{n}")
            } else {
                s.to_string()
            }
        })
        .collect();
    Some(parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboards::test_support::*;
    use chrono::Duration;

    fn kid_tt(days: &[(NaiveDate, Vec<Lesson>)]) -> Stamped<Timetable> {
        stamped(
            Timetable {
                weeks: vec![],
                days: days.iter().cloned().collect(),
            },
            warsaw(2026, 11, 25, 6, 0).with_timezone(&Utc),
        )
    }

    fn build_one(now: DateTime<Tz>, tt: &Stamped<Timetable>) -> KidCol {
        let cfg = config(1);
        let inputs = FamilyInputs {
            now,
            cfg: &cfg,
            calendar: None,
            weather: None,
            timetables: vec![Some(tt)],
        };
        build(&inputs).kids.remove(0)
    }

    fn standard_tt() -> Stamped<Timetable> {
        let mut wed = vec![lesson(8, 0, "Matematyka"), lesson(13, 40, "WF")];
        wed[1].end = time(14, 25);
        let mut thu = vec![lesson(8, 55, "Fizyka"), lesson(12, 45, "Chemia")];
        thu[1].end = time(13, 30);
        kid_tt(&[
            (date(2026, 11, 25), wed),
            (date(2026, 11, 26), thu),
            (date(2026, 11, 27), vec![lesson(8, 0, "Muzyka")]),
            (date(2026, 11, 30), vec![lesson(9, 0, "Plastyka")]),
        ])
    }

    #[test]
    fn before_switch_features_today_and_summarizes_next_day() {
        let kid = build_one(warsaw(2026, 11, 25, 7, 0), &standard_tt());
        let f = kid.featured.unwrap();
        assert_eq!(f.title, "Dziś · 8:00–14:25");
        assert_eq!(f.lessons.len(), 2);
        assert_eq!(f.lessons[0].time, "8:00");
        let s = kid.summary.unwrap();
        assert_eq!(s.title, "czw. · 8:55–13:30");
        assert_eq!(s.subjects.as_deref(), Some("Fizyka, Chemia"));
        assert_eq!(kid.label, "KID 1");
        assert_eq!(kid.empty, None);
        assert_eq!(kid.note, None);
    }

    #[test]
    fn after_switch_features_next_day_and_shows_today_times() {
        let kid = build_one(warsaw(2026, 11, 25, 19, 0), &standard_tt());
        assert_eq!(kid.featured.unwrap().title, "Jutro · 8:55–13:30");
        let s = kid.summary.unwrap();
        assert_eq!(s.title, "Dziś · 8:00–14:25");
        assert_eq!(s.subjects, None);
    }

    #[test]
    fn switch_boundary_is_inclusive() {
        let kid = build_one(warsaw(2026, 11, 25, 18, 0), &standard_tt());
        assert_eq!(kid.featured.unwrap().title, "Jutro · 8:55–13:30");
    }

    #[test]
    fn friday_evening_features_monday() {
        let kid = build_one(warsaw(2026, 11, 27, 19, 0), &standard_tt());
        assert_eq!(kid.featured.unwrap().title, "pon. · 9:00–9:45");
        assert_eq!(kid.summary.unwrap().title, "Dziś · 8:00–8:45");
    }

    #[test]
    fn holiday_week_shows_no_lessons() {
        let kid = build_one(warsaw(2026, 11, 25, 7, 0), &kid_tt(&[]));
        assert_eq!(kid.empty, Some("Brak lekcji"));
        assert!(kid.featured.is_none());
        assert!(kid.summary.is_none());
        assert_eq!(kid.note, None);
    }

    #[test]
    fn day_with_empty_lesson_list_is_not_a_school_day() {
        let tt = kid_tt(&[(date(2026, 11, 25), vec![])]);
        assert_eq!(next_school_day(&tt.data, date(2026, 11, 24)), None);
    }

    #[test]
    fn no_lessons_today_morning_features_next_school_day() {
        let kid = build_one(warsaw(2026, 11, 28, 7, 0), &standard_tt());
        assert_eq!(kid.featured.unwrap().title, "pon. · 9:00–9:45");
        let s = kid.summary;
        assert_eq!(s, None);
    }

    #[test]
    fn no_lessons_after_switch_has_no_summary() {
        let kid = build_one(warsaw(2026, 11, 28, 19, 0), &standard_tt());
        assert_eq!(kid.featured.unwrap().title, "pon. · 9:00–9:45");
        assert!(kid.summary.is_none());
    }

    #[test]
    fn next_school_day_is_strictly_after() {
        let tt = standard_tt();
        assert_eq!(
            next_school_day(&tt.data, date(2026, 11, 25)),
            Some(date(2026, 11, 26))
        );
        assert_eq!(
            next_school_day(&tt.data, date(2026, 11, 27)),
            Some(date(2026, 11, 30))
        );
        assert_eq!(next_school_day(&tt.data, date(2026, 11, 30)), None);
    }

    #[test]
    fn repeats_collapse() {
        let tt = kid_tt(&[
            (date(2026, 11, 25), vec![lesson(8, 0, "X")]),
            (
                date(2026, 11, 26),
                vec![
                    lesson(8, 0, "Ed. wczesnoszkolna"),
                    lesson(8, 55, "Ed. wczesnoszkolna"),
                    lesson(9, 50, "Muzyka"),
                    lesson(10, 55, "Ed. wczesnoszkolna"),
                ],
            ),
        ]);
        let kid = build_one(warsaw(2026, 11, 25, 7, 0), &tt);
        assert_eq!(
            kid.summary.unwrap().subjects.as_deref(),
            Some("Ed. wczesnoszkolna ×2, Muzyka, Ed. wczesnoszkolna")
        );
    }

    #[test]
    fn summary_skips_cancelled_lessons_but_title_spans_them() {
        let mut cancelled = lesson(8, 0, "Odwołana");
        cancelled.cancelled = true;
        let tt = kid_tt(&[
            (date(2026, 11, 25), vec![lesson(8, 0, "X")]),
            (date(2026, 11, 26), vec![cancelled, lesson(8, 55, "Fizyka")]),
        ]);
        let s = build_one(warsaw(2026, 11, 25, 7, 0), &tt).summary.unwrap();
        assert_eq!(s.title, "czw. · 8:00–9:40");
        assert_eq!(s.subjects.as_deref(), Some("Fizyka"));
    }

    #[test]
    fn cancelled_and_flag_pass_through() {
        let mut a = lesson(8, 0, "Historia");
        a.cancelled = true;
        let mut b = lesson(8, 55, "Biologia");
        b.flag = Some("zastępstwo".into());
        let tt = kid_tt(&[(date(2026, 11, 25), vec![a, b, lesson(9, 50, "WF")])]);
        let f = build_one(warsaw(2026, 11, 25, 7, 0), &tt).featured.unwrap();
        assert!(f.lessons[0].cancelled);
        assert_eq!(f.lessons[0].flag, None);
        assert!(!f.lessons[1].cancelled);
        assert_eq!(f.lessons[1].flag.as_deref(), Some("zast."));
        assert_eq!(f.lessons[1].subject, "Biologia");
        assert_eq!(f.lessons[2].flag, None);
    }

    #[test]
    fn kids_are_independent_and_missing_timetable_is_unavailable() {
        let cfg = config(2);
        let tt = standard_tt();
        let now = warsaw(2026, 11, 25, 7, 0);
        let inputs = FamilyInputs {
            now,
            cfg: &cfg,
            calendar: None,
            weather: None,
            timetables: vec![Some(&tt)],
        };
        let kids = build(&inputs).kids;
        assert_eq!(kids.len(), 2);
        assert!(kids[0].featured.is_some());
        assert_eq!(kids[1].label, "KID 2");
        assert!(kids[1].featured.is_none());
        assert!(kids[1].summary.is_none());
        assert!(kids[1].empty.is_none());
        assert_eq!(kids[1].note.as_deref(), Some("Plan lekcji niedostępny"));
    }

    #[test]
    fn stale_timetable_sets_note_and_keeps_data() {
        let mut tt = standard_tt();
        tt.fetched_at = warsaw(2026, 11, 24, 4, 0).with_timezone(&Utc);
        let kid = build_one(warsaw(2026, 11, 25, 7, 0), &tt);
        assert_eq!(kid.note.as_deref(), Some("stan na wt. 4:00"));
        assert!(kid.featured.is_some());
    }

    fn cal_view(now: DateTime<Tz>, events: &[String], days: u32) -> CalendarCol {
        cal_view_fetched(now, events, days, Duration::zero())
    }

    fn cal_view_fetched(
        now: DateTime<Tz>,
        events: &[String],
        days: u32,
        age: Duration,
    ) -> CalendarCol {
        let mut cfg = config(0);
        cfg.calendar_days = days;
        let cal = stamped(ics(events), now.with_timezone(&Utc) - age);
        let inputs = FamilyInputs {
            now,
            cfg: &cfg,
            calendar: Some(&cal),
            weather: None,
            timetables: vec![],
        };
        build(&inputs).calendar
    }

    fn line(time: &str, title: &str, location: Option<&str>) -> EventLine {
        EventLine {
            time: time.into(),
            title: title.into(),
            location: location.map(Into::into),
        }
    }

    fn week_labels(c: &CalendarCol) -> Vec<&str> {
        c.week
            .as_ref()
            .unwrap()
            .days
            .iter()
            .map(|d| d.label.as_str())
            .collect()
    }

    #[test]
    fn before_switch_features_today_and_lists_the_rest_of_the_week() {
        let events = [
            vevent(1, "Zakończone", "20261125T150000", "20261125T160000", false),
            located(
                vevent(2, "Trwa", "20261125T163000", "20261125T173000", false),
                "Dom\\nul. Długa 1",
            ),
            vevent(3, "Babcia", "20261126", "20261127", true),
            located(
                vevent(4, "Pianino", "20261126T171500", "20261126T181500", false),
                "Szkoła muzyczna",
            ),
            vevent(5, "Poza oknem", "20261202T100000", "20261202T110000", false),
            vevent(6, "   ", "20261126T100000", "20261126T110000", false),
        ];
        let c = cal_view(warsaw(2026, 11, 25, 17, 0), &events, 7);
        assert_eq!(c.heading, "DZISIAJ");
        assert_eq!(c.featured, vec![line("16:30–17:30", "Trwa", Some("Dom"))]);
        assert_eq!(c.featured_empty, None);
        let week = c.week.unwrap();
        assert_eq!(week.heading, "TEN TYDZIEŃ");
        assert_eq!(week.empty, None);
        assert_eq!(week.days.len(), 1);
        assert_eq!(week.days[0].label, "Jutro");
        assert_eq!(
            week.days[0].events,
            vec![
                line("cały dzień", "Babcia", None),
                line("17:15–18:15", "Pianino", None),
            ]
        );
        assert_eq!(c.more, None);
        assert_eq!(c.note, None);
    }

    fn evening_events() -> Vec<String> {
        vec![
            located(
                vevent(1, "Wywiadówka", "20261125T180000", "20261125T200000", false),
                "SP 12",
            ),
            vevent(2, "Imieniny", "20261125", "20261126", true),
            vevent(3, "Babcia", "20261126", "20261127", true),
            located(
                vevent(4, "Pianino", "20261126T171500", "20261126T181500", false),
                "Szkoła muzyczna",
            ),
            vevent(
                5,
                "Nocny pociąg",
                "20261125T220000",
                "20261126T020000",
                false,
            ),
            vevent(6, "Kino", "20261127T190000", "20261127T210000", false),
        ]
    }

    #[test]
    fn after_switch_stays_on_today_while_an_event_is_left() {
        let c = cal_view(warsaw(2026, 11, 25, 19, 0), &evening_events(), 7);
        assert_eq!(c.heading, "DZISIAJ");
        assert_eq!(
            c.featured,
            vec![
                line("cały dzień", "Imieniny", None),
                line("18:00–20:00", "Wywiadówka", Some("SP 12")),
                line("od 22:00", "Nocny pociąg", None),
            ]
        );
        assert_eq!(week_labels(&c), vec!["Jutro", "pt. 27"]);
    }

    #[test]
    fn after_switch_features_tomorrow_once_today_is_done() {
        // Wywiadówka ends at 20:00; the all-day event and the overnight train
        // do not hold off the switch.
        let c = cal_view(warsaw(2026, 11, 25, 20, 0), &evening_events(), 7);
        assert_eq!(c.heading, "JUTRO");
        assert_eq!(
            c.featured,
            vec![
                line("cały dzień", "Babcia", None),
                line("do 2:00", "Nocny pociąg", None),
                line("17:15–18:15", "Pianino", Some("Szkoła muzyczna")),
            ]
        );
        assert_eq!(week_labels(&c), vec!["Dziś", "pt. 27"]);
        let days = &c.week.unwrap().days;
        assert_eq!(days[0].events, vec![line("cały dzień", "Imieniny", None)]);
        assert_eq!(days[1].events, vec![line("19:00–21:00", "Kino", None)]);
    }

    #[test]
    fn one_day_window_still_features_tomorrow_after_the_switch() {
        let events = [
            vevent(1, "Pianino", "20261126T171500", "20261126T181500", false),
            vevent(
                2,
                "Nocny pociąg",
                "20261125T220000",
                "20261126T020000",
                false,
            ),
            vevent(3, "Kino", "20261127T190000", "20261127T210000", false),
        ];
        let c = cal_view(warsaw(2026, 11, 25, 20, 0), &events, 1);
        assert_eq!(c.heading, "JUTRO");
        assert_eq!(
            c.featured,
            vec![
                line("do 2:00", "Nocny pociąg", None),
                line("17:15–18:15", "Pianino", None),
            ]
        );
        assert_eq!(c.week.unwrap().empty, Some("Brak wydarzeń"));
        // Before the switch the window is just today.
        let c = cal_view(warsaw(2026, 11, 25, 7, 0), &events, 1);
        assert_eq!(c.heading, "DZISIAJ");
        assert_eq!(c.featured, vec![line("od 22:00", "Nocny pociąg", None)]);
        assert_eq!(c.week.unwrap().empty, Some("Brak wydarzeń"));
    }

    #[test]
    fn switch_waits_for_switch_at_even_when_today_is_done() {
        let c = cal_view(warsaw(2026, 11, 25, 17, 0), &[], 7);
        assert_eq!(c.heading, "DZISIAJ");
        let c = cal_view(warsaw(2026, 11, 25, 18, 0), &[], 7);
        assert_eq!(c.heading, "JUTRO");
    }

    #[test]
    fn events_crossing_midnight_get_open_ended_times() {
        let events = [
            vevent(1, "Obóz", "20261124T100000", "20261126T100000", false),
            vevent(2, "Noc", "20261124T200000", "20261125T090000", false),
            vevent(3, "Do północy", "20261125T220000", "20261126T000000", false),
            vevent(4, "Impreza", "20261125T230000", "20261126T010000", false),
        ];
        let c = cal_view(warsaw(2026, 11, 25, 7, 0), &events, 7);
        assert_eq!(
            c.featured,
            vec![
                line("cały dzień", "Obóz", None),
                line("do 9:00", "Noc", None),
                line("22:00–0:00", "Do północy", None),
                line("od 23:00", "Impreza", None),
            ]
        );
        assert!(c.week.unwrap().days.is_empty());
    }

    #[test]
    fn multi_day_all_day_event_is_listed_under_each_day() {
        let events = [vevent(1, "Wycieczka", "20261126", "20261129", true)];
        let c = cal_view(warsaw(2026, 11, 25, 7, 0), &events, 3);
        assert_eq!(c.featured_empty, Some("Brak wydarzeń"));
        assert!(c.featured.is_empty());
        assert_eq!(week_labels(&c), vec!["Jutro", "pt. 27"]);
    }

    #[test]
    fn empty_calendar_says_so_in_both_sections() {
        let c = cal_view(warsaw(2026, 11, 25, 7, 0), &[], 7);
        assert_eq!(c.featured_empty, Some("Brak wydarzeń"));
        let week = c.week.unwrap();
        assert!(week.days.is_empty());
        assert_eq!(week.empty, Some("Brak wydarzeń"));
        assert_eq!(c.more, None);
    }

    fn hourly_events(day: u32, n: usize, location: Option<&str>) -> Vec<String> {
        (0..n)
            .map(|i| {
                let ev = vevent(
                    i,
                    &format!("Wydarzenie {i}"),
                    &format!("202611{day}T{:02}0000", 8 + i),
                    &format!("202611{day}T{:02}3000", 8 + i),
                    false,
                );
                match location {
                    Some(l) => located(ev, l),
                    None => ev,
                }
            })
            .collect()
    }

    fn shown(c: &CalendarCol) -> usize {
        c.featured.len()
            + c.week
                .as_ref()
                .map_or(0, |w| w.days.iter().map(|d| d.events.len()).sum())
    }

    #[test]
    fn week_overflow_collapses_to_more_line() {
        let now = warsaw(2026, 11, 25, 7, 0);
        // "Brak wydarzeń" today (23px), week heading (38px) and day band (28px),
        // then 23px rows: 12 rows take 365px and fit the 370px column.
        let c = cal_view(now, &hourly_events(26, 12, Some("Hala")), 7);
        assert_eq!((shown(&c), c.more), (12, None));
        // The 15px stale note leaves 355px; reserving 25px for "+N więcej" leaves
        // a 330px budget, which fits 10.
        let stale = cal_view_fetched(now, &hourly_events(26, 12, None), 7, Duration::hours(3));
        assert!(stale.note.is_some());
        assert_eq!(shown(&stale), 10);
        assert_eq!(stale.more.as_deref(), Some("+2 więcej"));
        // 13 rows (388px) overflow: 11 fit the 345px budget.
        let c = cal_view(now, &hourly_events(26, 13, None), 7);
        assert_eq!(shown(&c), 11);
        assert_eq!(c.more.as_deref(), Some("+2 więcej"));
    }

    #[test]
    fn featured_rows_with_locations_take_room_first() {
        let now = warsaw(2026, 11, 25, 7, 0);
        // Located rows are 39px. 7 (273px) plus the empty week (61px) fit.
        let c = cal_view(now, &hourly_events(25, 7, Some("Hala")), 7);
        assert_eq!((shown(&c), c.more), (7, None));
        assert!(c.week.is_some());
        // 8 (312px) leave no room for the empty week, which is dropped silently.
        let c = cal_view(now, &hourly_events(25, 8, Some("Hala")), 7);
        assert_eq!((shown(&c), c.more), (8, None));
        assert!(c.week.is_none());
        // 9 (351px) still fit; the empty week has no room and must not push
        // an event into "+N więcej".
        let c = cal_view(now, &hourly_events(25, 9, Some("Hala")), 7);
        assert_eq!((shown(&c), c.more), (9, None));
        assert!(c.week.is_none());
        // 10 (390px) overflow the 345px budget: 8 fit.
        let c = cal_view(now, &hourly_events(25, 10, Some("Hala")), 7);
        assert_eq!(shown(&c), 8);
        assert_eq!(c.more.as_deref(), Some("+2 więcej"));
        // A week event that no longer fits is counted as hidden.
        let mut events = hourly_events(25, 8, Some("Hala"));
        events.push(vevent(
            99,
            "Jutro",
            "20261126T100000",
            "20261126T110000",
            false,
        ));
        let c = cal_view(now, &events, 7);
        assert_eq!(shown(&c), 8);
        assert_eq!(c.more.as_deref(), Some("+1 więcej"));
    }

    #[test]
    fn weather_detail_before_and_after_switch() {
        let now = warsaw(2026, 11, 25, 7, 0);
        let s = sample_inputs(now);
        let h = family_header(&s, now);
        assert_eq!(h.date, "Środa, 25 listopada");
        assert_eq!(h.temp.as_deref(), Some("4°"));
        assert_eq!(h.condition.as_deref(), Some("Pochmurno"));
        assert_eq!(h.icon, Some("cloudy"));
        assert_eq!(
            h.detail.as_deref(),
            Some("maks. 6° min. 1° · deszcz 30% od 15:00")
        );
        assert_eq!(h.note, None);

        let now = warsaw(2026, 11, 25, 19, 0);
        let s = sample_inputs(now);
        let h = family_header(&s, now);
        assert_eq!(
            h.detail.as_deref(),
            Some("jutro: maks. 3° min. -1° · śnieg")
        );
    }

    #[test]
    fn weather_fetched_yesterday_is_matched_by_date() {
        // Forecast covers the 25th and 26th; it is now the 26th, then the 27th.
        let now = warsaw(2026, 11, 26, 7, 0);
        let s = sample_inputs(now);
        assert_eq!(
            family_header(&s, now).detail.as_deref(),
            Some("maks. 3° min. -1°")
        );
        let now = warsaw(2026, 11, 26, 19, 0);
        let s = sample_inputs(now);
        assert_eq!(family_header(&s, now).detail, None);
        let now = warsaw(2026, 11, 27, 7, 0);
        let s = sample_inputs(now);
        assert_eq!(family_header(&s, now).detail, None);
    }

    #[test]
    fn precip_before_current_hour_is_ignored() {
        let now = warsaw(2026, 11, 25, 16, 0);
        let s = sample_inputs(now);
        assert_eq!(
            family_header(&s, now).detail.as_deref(),
            Some("maks. 6° min. 1°")
        );
        let now = warsaw(2026, 11, 25, 15, 20);
        let s = sample_inputs(now);
        assert!(
            family_header(&s, now)
                .detail
                .unwrap()
                .ends_with("deszcz 30% od 15:00")
        );
    }

    fn family_header(s: &Sample, now: DateTime<Tz>) -> Header {
        build(&s.inputs(now)).header
    }

    #[test]
    fn icons_cover_all_conditions() {
        let cases = [
            (Condition::Clear, "clear"),
            (Condition::PartlyCloudy, "partly-cloudy"),
            (Condition::Cloudy, "cloudy"),
            (Condition::Fog, "fog"),
            (Condition::Rain, "rain"),
            (Condition::Snow, "snow"),
            (Condition::Storm, "storm"),
        ];
        for (c, name) in cases {
            assert_eq!(icon(c), name);
        }
    }

    #[test]
    fn stale_and_missing_sources_set_notes() {
        let now = warsaw(2026, 11, 25, 12, 0);
        let mut s = sample_inputs(now);
        s.weather.as_mut().unwrap().fetched_at = now.with_timezone(&Utc) - Duration::hours(4);
        s.calendar = None;
        let v = build(&s.inputs(now));
        assert_eq!(v.header.note.as_deref(), Some("stan na 8:00"));
        assert!(v.header.temp.is_some());
        assert_eq!(v.calendar.note.as_deref(), Some("Kalendarz niedostępny"));
        assert!(v.calendar.featured.is_empty());
        assert_eq!(v.calendar.featured_empty, None);
        assert!(v.calendar.week.is_none());

        s.weather = None;
        let v = build(&s.inputs(now));
        assert_eq!(v.header.note.as_deref(), Some("Pogoda niedostępna"));
        assert_eq!(v.header.temp, None);
        assert_eq!(v.header.icon, None);
        assert_eq!(v.header.detail, None);
        assert_eq!(v.header.date, "Środa, 25 listopada");
    }

    #[test]
    fn stale_calendar_keeps_data_with_note() {
        let now = warsaw(2026, 11, 25, 12, 0);
        let mut s = sample_inputs(now);
        s.calendar.as_mut().unwrap().fetched_at = now.with_timezone(&Utc) - Duration::hours(3);
        let v = build(&s.inputs(now));
        assert_eq!(v.calendar.note.as_deref(), Some("stan na 9:00"));
        assert!(!v.calendar.featured.is_empty());
    }

    #[test]
    fn fixtures_build_and_serialize() {
        let m = sample_view_morning();
        assert_eq!(m.kids.len(), 2);
        assert_eq!(
            m.kids[0].featured.as_ref().unwrap().title,
            "Dziś · 8:00–14:25"
        );
        let e = sample_view_evening();
        assert_eq!(
            e.kids[1].featured.as_ref().unwrap().title,
            "Jutro · 8:00–11:40"
        );
        let w = worst_case_view();
        assert_eq!(w.calendar.more.as_deref(), Some("+5 więcej"));
        assert!(w.calendar.note.is_some());
        assert_eq!(w.kids[0].featured.as_ref().unwrap().lessons.len(), 9);
        serde_json::to_string(&w).unwrap();
    }
}
