//! iCloud ICS calendar source.
//!
//! The raw feed is cached as an [`IcsSnapshot`] and expanded into concrete
//! events at view time with [`events_in`], so a `304 Not Modified` response
//! never leaves the displayed window out of date.
//!
//! Parsing uses `icalendar`'s low-level parser. Recurrence rules are expanded
//! by `rrule`, but `UNTIL` and `EXDATE` are applied here: iCloud mixes `DATE`,
//! floating and UTC forms that `rrule` rejects when they differ from `DTSTART`.
//! `VTIMEZONE` blocks are ignored; IANA `TZID`s resolve through `chrono-tz`.

use std::collections::HashSet;
use std::str::FromStr;

use chrono::{DateTime, Duration, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use icalendar::parser::{self, Component};
use rrule::RRuleSet;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::SourceError;

/// The parser's own diagnostic quotes the failing input line, so it is dropped.
const INVALID_ICS: SourceError = SourceError::Parse("calendar is not valid iCalendar");

/// Safety cap on occurrences examined per recurring event.
const MAX_OCCURRENCES: usize = 20_000;

/// Raw ICS body plus the validator used for conditional refetches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IcsSnapshot {
    pub text: String,
    pub etag: Option<String>,
}

#[derive(Debug)]
pub enum FetchOutcome {
    NotModified,
    Fresh(IcsSnapshot),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CalendarEvent {
    pub title: String,
    /// First line of `LOCATION`; iCloud puts the street address on later lines.
    pub location: Option<String>,
    pub when: EventWhen,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EventWhen {
    /// `last` is inclusive.
    AllDay { first: NaiveDate, last: NaiveDate },
    Timed {
        start: DateTime<Tz>,
        end: DateTime<Tz>,
    },
}

/// Downloads the feed, sending `If-None-Match` when an etag is known.
pub async fn fetch(
    client: &reqwest::Client,
    url: &str,
    etag: Option<&str>,
) -> Result<FetchOutcome, SourceError> {
    let mut req = client.get(url);
    if let Some(etag) = etag {
        req = req.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let resp = req.send().await?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(FetchOutcome::NotModified);
    }
    if !status.is_success() {
        return Err(SourceError::Status(status.as_u16()));
    }
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let text = resp.text().await?;
    Ok(FetchOutcome::Fresh(IcsSnapshot { text, etag }))
}

/// Checks that `ics` is one whole VCALENDAR (it may hold no events), so a captive
/// page, an error body, a bare component or a truncated feed served with 200
/// never replaces the last good feed.
pub fn validate(ics: &str) -> Result<(), SourceError> {
    let unfolded = parser::unfold(ics);
    let mut lines = unfolded.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or_default();
    let last = lines.next_back().unwrap_or_default();
    if !first.eq_ignore_ascii_case("BEGIN:VCALENDAR") || !last.eq_ignore_ascii_case("END:VCALENDAR")
    {
        return Err(SourceError::Parse("not a VCALENDAR"));
    }
    parser::read_calendar(&unfolded).map_err(|_| INVALID_ICS)?;
    Ok(())
}

/// A parsed DTSTART/DTEND/RECURRENCE-ID/EXDATE value.
#[derive(Debug, Clone, Copy)]
enum Moment {
    Date(NaiveDate),
    At(DateTime<Tz>),
}

#[derive(Debug)]
struct RawEvent {
    uid: String,
    title: String,
    location: Option<String>,
    start: Moment,
    /// Length: whole days for all-day events, otherwise a real duration.
    span: Span,
    rrule: Option<String>,
    exdates: Vec<Moment>,
    recurrence_id: Option<Moment>,
    /// `STATUS:CANCELLED`: never shown, but an override still hides its occurrence.
    cancelled: bool,
}

#[derive(Debug, Clone, Copy)]
enum Span {
    Days(i64),
    Time(Duration),
}

/// Expands the feed into events overlapping `[from 00:00, to 24:00)` local time.
///
/// Sorted by start; all-day events come first within a day.
pub fn events_in(
    ics: &str,
    tz: Tz,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<CalendarEvent>, SourceError> {
    let unfolded = parser::unfold(ics);
    let cal = parser::read_calendar(&unfolded).map_err(|_| INVALID_ICS)?;

    let raws: Vec<RawEvent> = cal
        .components
        .iter()
        .filter(|c| c.name.as_str() == "VEVENT")
        .filter_map(|c| raw_event(c, tz))
        .collect();

    let overridden: HashSet<(String, String)> = raws
        .iter()
        .filter_map(|r| Some((r.uid.clone(), moment_key(r.recurrence_id?))))
        .collect();

    let win = Window {
        from,
        to,
        start: local_start(tz, from),
        end: local_start(tz, to.succ_opt().unwrap_or(to)),
    };

    let mut out = Vec::new();
    for raw in &raws {
        if raw.cancelled {
            continue;
        }
        if raw.recurrence_id.is_some() || raw.rrule.is_none() {
            push_if_overlaps(&mut out, raw, raw.start, tz, &win);
            continue;
        }
        for occ in occurrences(raw, tz, &win) {
            if overridden.contains(&(raw.uid.clone(), moment_key(occ))) {
                continue;
            }
            push_if_overlaps(&mut out, raw, occ, tz, &win);
        }
    }

    out.sort_by_key(|e| match &e.when {
        EventWhen::AllDay { first, .. } => (first.and_hms_opt(0, 0, 0), 0u8),
        EventWhen::Timed { start, .. } => (Some(start.naive_local()), 1u8),
    });
    Ok(out)
}

struct Window {
    from: NaiveDate,
    to: NaiveDate,
    start: DateTime<Tz>,
    end: DateTime<Tz>,
}

fn push_if_overlaps(
    out: &mut Vec<CalendarEvent>,
    raw: &RawEvent,
    occ: Moment,
    tz: Tz,
    win: &Window,
) {
    let when = match (occ, raw.span) {
        (Moment::Date(first), Span::Days(n)) => {
            let last = first + Duration::days((n - 1).max(0));
            if first > win.to || last < win.from {
                return;
            }
            EventWhen::AllDay { first, last }
        }
        (Moment::At(at), Span::Time(d)) => {
            let start = at.with_timezone(&tz);
            let end = start + d;
            let overlaps = if d.is_zero() {
                start >= win.start && start < win.end
            } else {
                start < win.end && end > win.start
            };
            if !overlaps {
                return;
            }
            EventWhen::Timed { start, end }
        }
        _ => return,
    };
    out.push(CalendarEvent {
        title: raw.title.clone(),
        location: raw.location.clone(),
        when,
    });
}

/// Occurrences of a recurring event that could touch the window.
fn occurrences(raw: &RawEvent, tz: Tz, win: &Window) -> Vec<Moment> {
    let Some(rule) = &raw.rrule else {
        return Vec::new();
    };
    // `RRuleSet` parses several property lines. A rule mistyped as TEXT comes
    // unescaped, so a line break in it would smuggle in extra lines (whose values
    // `rrule` may quote in its own warnings). Only a single rule line is accepted.
    if rule.chars().any(is_line_break) {
        warn!(source = "calendar", "skipping event with unusable RRULE");
        return Vec::new();
    }
    let (rule, until) = split_until(rule);
    let (dtstart_line, event_tz) = match raw.start {
        Moment::Date(d) => {
            let at = local_start(tz, d);
            (
                format!("DTSTART;TZID={}:{}", tz.name(), at.format("%Y%m%dT%H%M%S")),
                tz,
            )
        }
        Moment::At(at) if at.timezone() == Tz::UTC => {
            (format!("DTSTART:{}Z", at.format("%Y%m%dT%H%M%S")), Tz::UTC)
        }
        Moment::At(at) => (
            format!(
                "DTSTART;TZID={}:{}",
                at.timezone().name(),
                at.format("%Y%m%dT%H%M%S")
            ),
            at.timezone(),
        ),
    };
    let set = match RRuleSet::from_str(&format!("{dtstart_line}\nRRULE:{rule}")) {
        Ok(s) => s,
        Err(_) => {
            // Feed text (UID, the rrule error quoting the value) stays out of logs.
            warn!(source = "calendar", "skipping event with unusable RRULE");
            return Vec::new();
        }
    };

    let until_is_date = until.as_deref().is_some_and(|u| !u.contains('T'));
    let until_instant = until
        .and_then(|u| parse_moment(&u, None, tz))
        .map(|m| match m {
            Moment::Date(d) => local_start(tz, d.succ_opt().unwrap_or(d)),
            Moment::At(at) => at,
        });
    let excluded: Vec<Moment> = raw.exdates.clone();

    // Latest start that can still overlap the window.
    let horizon = win.end;
    let mut found = Vec::new();
    for (n, dt) in set.into_iter().enumerate() {
        if n >= MAX_OCCURRENCES || dt >= horizon {
            break;
        }
        let dt = dt.with_timezone(&event_tz);
        // UNTIL is inclusive for date-times and covers the whole day for dates.
        if let Some(u) = until_instant {
            let past = if until_is_date { dt >= u } else { dt > u };
            if past {
                break;
            }
        }
        let occ = match raw.start {
            Moment::Date(_) => Moment::Date(dt.date_naive()),
            Moment::At(_) => Moment::At(dt),
        };
        if excluded.iter().any(|x| same_occurrence(x, &occ, tz)) {
            continue;
        }
        found.push(occ);
    }
    found
}

/// CR, LF and the other characters Unicode treats as line delimiters.
fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r' | '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

/// Removes `UNTIL=` from an RRULE value, returning the rest and the raw value.
fn split_until(rule: &str) -> (String, Option<String>) {
    let mut until = None;
    let parts: Vec<&str> = rule
        .split(';')
        .filter(|p| match p.strip_prefix("UNTIL=") {
            Some(v) => {
                until = Some(v.to_owned());
                false
            }
            None => true,
        })
        .collect();
    (parts.join(";"), until)
}

fn same_occurrence(ex: &Moment, occ: &Moment, tz: Tz) -> bool {
    match (ex, occ) {
        (Moment::Date(a), Moment::Date(b)) => a == b,
        (Moment::At(a), Moment::At(b)) => a == b,
        // A DATE exclusion on a timed series removes that local day's occurrence.
        (Moment::Date(a), Moment::At(b)) => b.with_timezone(&tz).date_naive() == *a,
        (Moment::At(_), Moment::Date(_)) => false,
    }
}

/// Identity of an occurrence, used to match RECURRENCE-ID overrides.
fn moment_key(m: Moment) -> String {
    match m {
        Moment::Date(d) => d.to_string(),
        Moment::At(at) => at.with_timezone(&Utc).to_rfc3339(),
    }
}

fn local_start(tz: Tz, date: NaiveDate) -> DateTime<Tz> {
    resolve_local(tz, date.and_hms_opt(0, 0, 0).expect("midnight is valid"))
}

fn resolve_local(tz: Tz, naive: NaiveDateTime) -> DateTime<Tz> {
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(t) => t,
        LocalResult::Ambiguous(first, _) => first,
        // Inside a DST gap: the wall time does not exist, move past it.
        LocalResult::None => tz
            .from_local_datetime(&(naive + Duration::hours(1)))
            .earliest()
            .unwrap_or_else(|| tz.from_utc_datetime(&naive)),
    }
}

fn prop<'a>(c: &'a Component<'_>, name: &str) -> Option<&'a icalendar::parser::Property<'a>> {
    c.properties
        .iter()
        .find(|p| p.name.as_str().eq_ignore_ascii_case(name))
}

fn param<'a>(p: &'a icalendar::parser::Property<'_>, key: &str) -> Option<&'a str> {
    p.params
        .iter()
        .find(|q| q.key.as_str().eq_ignore_ascii_case(key))
        .and_then(|q| q.val.as_ref())
        .map(|v| v.as_str().trim_matches('"'))
}

fn parse_prop_moment(p: &icalendar::parser::Property<'_>, local: Tz) -> Option<Vec<Moment>> {
    let is_date = param(p, "VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE"));
    let tzid = param(p, "TZID");
    let moments: Vec<Moment> = p
        .val
        .as_str()
        .split(',')
        .filter_map(|v| {
            if is_date {
                parse_moment(v.trim(), None, local).filter(|m| matches!(m, Moment::Date(_)))
            } else {
                parse_moment(v.trim(), tzid, local)
            }
        })
        .collect();
    (!moments.is_empty()).then_some(moments)
}

/// Parses `YYYYMMDD`, `YYYYMMDDTHHMMSS` (floating or `TZID`) or `...Z` values.
fn parse_moment(value: &str, tzid: Option<&str>, local: Tz) -> Option<Moment> {
    if value.len() == 8 && !value.contains('T') {
        return NaiveDate::parse_from_str(value, "%Y%m%d")
            .ok()
            .map(Moment::Date);
    }
    if let Some(utc) = value.strip_suffix('Z') {
        let naive = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").ok()?;
        return Some(Moment::At(Tz::UTC.from_utc_datetime(&naive)));
    }
    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
    let zone = match tzid {
        None => local,
        Some(id) => id.parse::<Tz>().unwrap_or_else(|_| {
            warn!(
                source = "calendar",
                "unknown TZID, using the configured time zone"
            );
            local
        }),
    };
    Some(Moment::At(resolve_local(zone, naive)))
}

fn parse_duration(v: &str) -> Option<Duration> {
    let (neg, rest) = match v.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, v.strip_prefix('+').unwrap_or(v)),
    };
    let rest = rest.strip_prefix('P')?;
    let mut total = Duration::zero();
    let mut num = String::new();
    let mut in_time = false;
    for ch in rest.chars() {
        match ch {
            'T' => in_time = true,
            '0'..='9' => num.push(ch),
            unit => {
                let n: i64 = num.parse().ok()?;
                num.clear();
                total += match (unit, in_time) {
                    ('W', _) => Duration::weeks(n),
                    ('D', _) => Duration::days(n),
                    ('H', true) => Duration::hours(n),
                    ('M', true) => Duration::minutes(n),
                    ('S', true) => Duration::seconds(n),
                    _ => return None,
                };
            }
        }
    }
    Some(if neg { -total } else { total })
}

fn raw_event(c: &Component<'_>, local: Tz) -> Option<RawEvent> {
    let cancelled =
        prop(c, "STATUS").is_some_and(|p| p.val.as_str().eq_ignore_ascii_case("CANCELLED"));
    let start = parse_prop_moment(prop(c, "DTSTART")?, local)?
        .into_iter()
        .next()?;
    let end = prop(c, "DTEND")
        .and_then(|p| parse_prop_moment(p, local))
        .and_then(|m| m.into_iter().next());
    let dur = prop(c, "DURATION").and_then(|p| parse_duration(p.val.as_str()));

    let span = match start {
        Moment::Date(s) => {
            let days = match (end, dur) {
                (Some(Moment::Date(e)), _) => (e - s).num_days(),
                (_, Some(d)) => d.num_days(),
                _ => 1,
            };
            Span::Days(days.max(1))
        }
        Moment::At(s) => match (end, dur) {
            (Some(Moment::At(e)), _) => Span::Time((e - s).max(Duration::zero())),
            (Some(Moment::Date(e)), _) => {
                Span::Time((local_start(s.timezone(), e) - s).max(Duration::zero()))
            }
            (_, Some(d)) => Span::Time(d),
            _ => Span::Time(Duration::zero()),
        },
    };

    let exdates = c
        .properties
        .iter()
        .filter(|p| p.name.as_str().eq_ignore_ascii_case("EXDATE"))
        .filter_map(|p| parse_prop_moment(p, local))
        .flatten()
        .collect();

    Some(RawEvent {
        uid: prop(c, "UID")
            .map(|p| p.val.as_str().to_owned())
            .unwrap_or_default(),
        title: prop(c, "SUMMARY")
            .map(|p| p.val.clone().unescape_text().as_str().trim().to_owned())
            .unwrap_or_default(),
        location: prop(c, "LOCATION").and_then(|p| {
            let text = p.val.clone().unescape_text();
            text.as_str()
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_owned)
        }),
        start,
        span,
        rrule: prop(c, "RRULE").map(|p| p.val.as_str().to_owned()),
        exdates,
        cancelled,
        recurrence_id: prop(c, "RECURRENCE-ID")
            .and_then(|p| parse_prop_moment(p, local))
            .and_then(|m| m.into_iter().next()),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn feed_text_never_reaches_logs_or_errors() {
        use crate::sources::test_logs::Logs;
        let (logs, _guard) = Logs::capture();
        let tz = chrono_tz::Europe::Warsaw;
        let day = NaiveDate::from_ymd_opt(2026, 11, 25).unwrap();
        let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:SECRETSENTINEL-uid\n\
                   DTSTART;TZID=SECRETSENTINEL/Zone:20261125T100000\n\
                   RRULE:FREQ=SECRETSENTINEL\nSUMMARY:SECRETSENTINEL\nEND:VEVENT\n\
                   END:VCALENDAR\n";
        let events = events_in(ics, tz, day, day).unwrap();
        // An RRULE mistyped as TEXT has its escaped line break unescaped by the
        // parser, giving an extra property line whose unexpected date parameter
        // `rrule` would quote in a warning.
        let escaped = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:u2\n\
                       DTSTART:20261125T100000Z\n\
                       RRULE;VALUE=TEXT:FREQ=DAILY\\nRDATE;VALUE=SECRETSENTINEL:20261126T100000Z\n\
                       SUMMARY:Daily\nEND:VEVENT\nEND:VCALENDAR\n";
        let escaped_events = events_in(escaped, tz, day, day).unwrap();
        assert!(
            escaped_events.is_empty(),
            "{escaped_events:?} {}",
            logs.text()
        );
        let broken = "BEGIN:VCALENDAR\nEND:SECRETSENTINEL\nEND:VCALENDAR\n";
        let err = events_in(broken, tz, day, day).unwrap_err();
        let seen = format!("{err} {err:?} {events:?} {}", logs.text());
        assert!(logs.text().contains("unusable RRULE"), "{}", logs.text());
        assert!(!seen.contains("SECRETSENTINEL-uid"), "{seen}");
        // `rrule` lowercases the parameters it quotes.
        assert!(
            !logs.text().to_lowercase().contains("secretsentinel"),
            "{}",
            logs.text()
        );
        assert!(
            !format!("{err} {err:?}").contains("SECRETSENTINEL"),
            "{err}"
        );
    }

    use super::*;
    use chrono::Timelike;
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const WARSAW: Tz = chrono_tz::Europe::Warsaw;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("tests/fixtures/ics/{name}.ics")).unwrap()
    }

    fn timed(e: &CalendarEvent) -> (DateTime<Tz>, DateTime<Tz>) {
        match &e.when {
            EventWhen::Timed { start, end } => (*start, *end),
            other => panic!("expected timed event, got {other:?}"),
        }
    }

    #[test]
    fn single_timed_event() {
        let evs = events_in(
            &fixture("single_timed"),
            WARSAW,
            d(2026, 11, 25),
            d(2026, 11, 25),
        )
        .unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].title, "Dentist");
        assert_eq!(evs[0].location, None);
        let (s, e) = timed(&evs[0]);
        assert_eq!((s.hour(), s.minute()), (14, 30));
        assert_eq!((e.hour(), e.minute()), (15, 30));
        let none = events_in(
            &fixture("single_timed"),
            WARSAW,
            d(2026, 11, 26),
            d(2026, 11, 27),
        )
        .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn weekly_rrule_expands_in_window() {
        // Mondays at 17:00 from 2026-11-02; window Wed 25 Nov .. Tue 1 Dec.
        let evs = events_in(&fixture("weekly"), WARSAW, d(2026, 11, 25), d(2026, 12, 1)).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(timed(&evs[0]).0.date_naive(), d(2026, 11, 30));
        // Window covering two Mondays.
        let evs = events_in(&fixture("weekly"), WARSAW, d(2026, 11, 29), d(2026, 12, 7)).unwrap();
        let days: Vec<_> = evs.iter().map(|e| timed(e).0.date_naive()).collect();
        assert_eq!(days, vec![d(2026, 11, 30), d(2026, 12, 7)]);
    }

    #[test]
    fn exdate_removes_occurrence() {
        let evs = events_in(&fixture("exdate"), WARSAW, d(2026, 11, 25), d(2026, 12, 8)).unwrap();
        let days: Vec<_> = evs.iter().map(|e| timed(e).0.date_naive()).collect();
        assert_eq!(days, vec![d(2026, 12, 7)]);
    }

    #[test]
    fn recurrence_id_override_replaces_occurrence() {
        let evs = events_in(
            &fixture("override"),
            WARSAW,
            d(2026, 11, 25),
            d(2026, 12, 8),
        )
        .unwrap();
        let got: Vec<_> = evs
            .iter()
            .map(|e| {
                let (s, _) = timed(e);
                (e.title.clone(), s.date_naive(), s.hour())
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("Swim class (moved)".to_string(), d(2026, 12, 1), 19),
                ("Swim class".to_string(), d(2026, 12, 7), 17),
            ]
        );
    }

    #[test]
    fn cancelled_override_suppresses_the_occurrence() {
        let evs = events_in(
            &fixture("override_cancelled"),
            WARSAW,
            d(2026, 11, 25),
            d(2026, 12, 8),
        )
        .unwrap();
        let days: Vec<_> = evs.iter().map(|e| timed(e).0.date_naive()).collect();
        assert_eq!(days, vec![d(2026, 12, 7)]);
    }

    #[test]
    fn weekly_event_keeps_local_hour_across_dst_end() {
        // Clocks go back on 2026-10-25.
        let evs = events_in(
            &fixture("dst_weekly"),
            WARSAW,
            d(2026, 10, 19),
            d(2026, 11, 2),
        )
        .unwrap();
        let got: Vec<_> = evs
            .iter()
            .map(|e| {
                let s = timed(e).0;
                (s.date_naive(), s.hour(), s.minute())
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (d(2026, 10, 19), 16, 0),
                (d(2026, 10, 26), 16, 0),
                (d(2026, 11, 2), 16, 0),
            ]
        );
    }

    #[test]
    fn only_a_whole_vcalendar_is_a_calendar() {
        assert!(validate("<html><body>Login</body></html>").is_err());
        assert!(validate("").is_err());
        let todo = "BEGIN:VTODO\r\nUID:1\r\nSUMMARY:x\r\nEND:VTODO\r\n";
        assert!(validate(todo).is_err(), "a bare VTODO is not a calendar");
        let truncated = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:1\r\n";
        assert!(validate(truncated).is_err(), "a truncated feed is rejected");
        let empty = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:x\r\nEND:VCALENDAR\r\n";
        assert!(validate(empty).is_ok(), "an empty calendar is fine");
        assert!(validate(&fixture("weekly")).is_ok());
    }

    #[test]
    fn utc_tzid_and_floating_times_convert_to_local() {
        let evs = events_in(
            &fixture("timezones"),
            WARSAW,
            d(2026, 11, 25),
            d(2026, 11, 25),
        )
        .unwrap();
        assert_eq!(evs.len(), 4);
        for e in &evs {
            let (s, _) = timed(e);
            assert_eq!((s.hour(), s.minute()), (16, 0), "{}", e.title);
            assert_eq!(s.timezone(), WARSAW);
        }
    }

    #[test]
    fn multi_day_all_day_event_listed_each_day() {
        let evs = events_in(
            &fixture("all_day"),
            WARSAW,
            d(2026, 11, 27),
            d(2026, 11, 27),
        )
        .unwrap();
        let trip = evs.iter().find(|e| e.title == "School trip").unwrap();
        assert_eq!(
            trip.when,
            EventWhen::AllDay {
                first: d(2026, 11, 26),
                last: d(2026, 11, 28)
            }
        );
    }

    #[test]
    fn recurring_all_day_events_expand() {
        // Birthday yearly 14 Mar; Trash day: Mondays until 31 Dec.
        let evs = events_in(&fixture("all_day"), WARSAW, d(2027, 3, 14), d(2027, 3, 14)).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].title, "Birthday");
        assert_eq!(
            evs[0].when,
            EventWhen::AllDay {
                first: d(2027, 3, 14),
                last: d(2027, 3, 14)
            }
        );

        let evs = events_in(&fixture("all_day"), WARSAW, d(2026, 12, 28), d(2027, 1, 10)).unwrap();
        let trash: Vec<_> = evs.iter().filter(|e| e.title == "Trash day").collect();
        assert_eq!(trash.len(), 1, "UNTIL=20261231 excludes January Mondays");
        assert_eq!(
            trash[0].when,
            EventWhen::AllDay {
                first: d(2026, 12, 28),
                last: d(2026, 12, 28)
            }
        );
    }

    #[test]
    fn overnight_event_listed_on_start_day() {
        let ics = fixture("overnight");
        let evs = events_in(&ics, WARSAW, d(2026, 11, 25), d(2026, 11, 25)).unwrap();
        assert_eq!(evs.len(), 1);
        let (s, e) = timed(&evs[0]);
        assert_eq!(s.date_naive(), d(2026, 11, 25));
        assert_eq!(e.date_naive(), d(2026, 11, 26));
        // Still running at the start of the next day's window.
        let next = events_in(&ics, WARSAW, d(2026, 11, 26), d(2026, 11, 26)).unwrap();
        assert_eq!(next.len(), 1);
        let later = events_in(&ics, WARSAW, d(2026, 11, 27), d(2026, 11, 27)).unwrap();
        assert!(later.is_empty());
    }

    #[test]
    fn all_day_sorts_before_timed_on_same_day() {
        let mut ics = fixture("single_timed");
        ics = ics.replace("END:VCALENDAR\r\n", "");
        ics.push_str("BEGIN:VEVENT\r\nUID:x@t\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:20261125\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
        let evs = events_in(&ics, WARSAW, d(2026, 11, 25), d(2026, 11, 25)).unwrap();
        let titles: Vec<_> = evs.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["Holiday", "Dentist"]);
    }

    #[test]
    fn location_keeps_first_non_empty_line() {
        let ics = fixture("single_timed").replace(
            "SUMMARY:Dentist\r\n",
            "SUMMARY:Dentist\r\nLOCATION:\\nKino Muranów\\nul. Gen. Andersa 5\\, Warszawa\r\n",
        );
        let evs = events_in(&ics, WARSAW, d(2026, 11, 25), d(2026, 11, 25)).unwrap();
        assert_eq!(evs[0].location.as_deref(), Some("Kino Muranów"));
        let blank = fixture("single_timed")
            .replace("SUMMARY:Dentist\r\n", "SUMMARY:Dentist\r\nLOCATION: \r\n");
        let evs = events_in(&blank, WARSAW, d(2026, 11, 25), d(2026, 11, 25)).unwrap();
        assert_eq!(evs[0].location, None);
    }

    #[test]
    fn garbage_is_a_parse_error() {
        assert!(matches!(
            events_in(
                "BEGIN:VCALENDAR\r\nBEGIN:",
                WARSAW,
                d(2026, 11, 25),
                d(2026, 11, 25)
            ),
            Err(SourceError::Parse(_))
        ));
    }

    #[tokio::test]
    async fn fetch_sends_if_none_match_and_handles_304() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(header("if-none-match", "\"v1\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v2\"")
                    .set_body_string("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n"),
            )
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let url = format!("{}/cal.ics", server.uri());

        let out = fetch(&client, &url, Some("\"v1\"")).await.unwrap();
        assert!(matches!(out, FetchOutcome::NotModified));

        match fetch(&client, &url, None).await.unwrap() {
            FetchOutcome::Fresh(s) => {
                assert_eq!(s.etag.as_deref(), Some("\"v2\""));
                assert!(s.text.starts_with("BEGIN:VCALENDAR"));
            }
            other => panic!("expected fresh, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_error_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let r = fetch(&reqwest::Client::new(), &server.uri(), None).await;
        assert!(matches!(r, Err(SourceError::Status(500))));
    }
}
