//! Librus timetable page parsing.

use std::collections::BTreeMap;

use chrono::{Days, NaiveDate, NaiveTime};
use scraper::{ElementRef, Html, Node, Selector};
use serde::{Deserialize, Serialize};

use super::auth::LibrusClient;
use crate::sources::SourceError;

/// One lesson cell of the timetable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lesson {
    pub start: NaiveTime,
    pub end: NaiveTime,
    pub subject: String,
    pub teacher: Option<String>,
    pub room: Option<String>,
    pub cancelled: bool,
    /// Librus info label, e.g. `zastępstwo`.
    pub flag: Option<String>,
}

/// Lessons of one or more weeks. Only days that have lessons are present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timetable {
    /// Mondays of the weeks covered.
    pub weeks: Vec<NaiveDate>,
    pub days: BTreeMap<NaiveDate, Vec<Lesson>>,
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("valid selector")
}

/// Collapses whitespace runs (including nbsp) into single spaces.
fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first two `H:MM` / `HH:MM` times in `s`.
fn parse_times(s: &str) -> Option<(NaiveTime, NaiveTime)> {
    let b = s.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b':' && i + 2 < b.len() && b[i + 1].is_ascii_digit() && b[i + 2].is_ascii_digit()
        {
            let mut h = i;
            while h > 0 && b[h - 1].is_ascii_digit() && i - h < 2 {
                h -= 1;
            }
            if h < i
                && let (Ok(hh), Ok(mm)) = (s[h..i].parse::<u32>(), s[i + 1..i + 3].parse::<u32>())
                && let Some(t) = NaiveTime::from_hms_opt(hh, mm, 0)
            {
                found.push(t);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut it = found.into_iter();
    Some((it.next()?, it.next()?))
}

/// Drops a trailing parenthesised group suffix, e.g. `Name (5c grupa 1)` → `Name`.
fn strip_group(teacher: &str) -> &str {
    match teacher.strip_suffix(')').and_then(|t| t.rsplit_once('(')) {
        Some((name, _)) => name.trim(),
        None => teacher,
    }
}

/// `(subject, teacher, room)` of a `.text` element.
fn parse_text(el: ElementRef) -> (String, Option<String>, Option<String>) {
    let subject = el
        .select(&sel("b"))
        .next()
        .map(|b| squash(&b.text().collect::<String>()))
        .unwrap_or_default();
    // Text after the first <br>.
    let mut after = String::new();
    let mut seen_br = false;
    for node in el.descendants() {
        match node.value() {
            Node::Element(e) if e.name() == "br" => seen_br = true,
            Node::Text(t) if seen_br => after.push_str(t),
            _ => {}
        }
    }
    // Real pages use `&nbsp` without a semicolon, a leading `-`, and a group suffix:
    // `-Name&nbsp;(5c grupa 1)&nbsp&nbsp;s.&nbsp;9`. Squashing turns every nbsp into
    // a plain space first.
    let after = squash(&after);
    let after = after.strip_prefix('-').map_or(after.as_str(), str::trim);
    let (teacher, room) = match after.split_once(" s. ") {
        Some((t, r)) => (t.trim(), r.trim()),
        None => (after, ""),
    };
    let teacher = strip_group(teacher);
    let opt = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    (subject, opt(teacher), opt(room))
}

/// Parses one timetable page into its Monday and the lessons per date.
///
/// A page without `table.plan-lekcji` is [`SourceError::NotTimetable`]; a table
/// without lessons is an empty week (a holiday).
pub fn parse_week(
    html: &str,
) -> Result<(NaiveDate, BTreeMap<NaiveDate, Vec<Lesson>>), SourceError> {
    let doc = Html::parse_document(html);
    if doc.select(&sel("table.plan-lekcji")).next().is_none() {
        return Err(SourceError::NotTimetable);
    }
    let monday = week_options(html)
        .into_iter()
        .find(|(_, selected)| *selected)
        .and_then(|(v, _)| NaiveDate::parse_from_str(v.get(..10)?, "%Y-%m-%d").ok())
        .ok_or_else(|| SourceError::Parse("no selected week in the tydzien select"))?;

    let (row, th, td, text, info) = (
        sel("table.plan-lekcji tr.line1"),
        sel("th"),
        sel("td"),
        sel(".text"),
        sel(".plan-lekcji-info"),
    );
    let mut days: BTreeMap<NaiveDate, Vec<Lesson>> = BTreeMap::new();
    for tr in doc.select(&row) {
        let Some(header) = tr.select(&th).next() else {
            continue;
        };
        let Some((start, end)) = parse_times(&header.text().collect::<String>()) else {
            continue;
        };
        let cells: Vec<_> = tr.select(&td).collect();
        if cells.len() < 2 {
            continue;
        }
        for (idx, cell) in cells[1..cells.len() - 1].iter().enumerate() {
            let entries: Vec<_> = cell
                .select(&text)
                .filter(|t| !t.text().collect::<String>().trim().is_empty())
                .map(|t| {
                    let struck = t
                        .ancestors()
                        .filter_map(ElementRef::wrap)
                        .any(|a| a.value().name() == "s");
                    (parse_text(t), struck)
                })
                .collect();
            // One lesson per active entry (group splits); a replaced lesson also
            // shows its struck original, which is dropped. With no active entry the
            // first struck one stands in as cancelled.
            let active: Vec<_> = entries.iter().filter(|(_, struck)| !struck).collect();
            let chosen: Vec<_> = if active.is_empty() {
                entries.iter().take(1).collect()
            } else {
                active
            };
            let flags: Vec<String> = cell
                .select(&info)
                .map(|i| squash(&i.text().collect::<String>()))
                .filter(|s| !s.is_empty())
                .collect();
            let Some(date) = monday.checked_add_days(Days::new(idx as u64)) else {
                continue;
            };
            for ((subject, teacher, room), struck) in chosen {
                if subject.is_empty() {
                    continue;
                }
                days.entry(date).or_default().push(Lesson {
                    start,
                    end,
                    subject: subject.clone(),
                    teacher: teacher.clone(),
                    room: room.clone(),
                    cancelled: *struck,
                    flag: (!flags.is_empty()).then(|| flags.join(", ")),
                });
            }
        }
    }
    Ok((monday, days))
}

/// Names in a lesson tooltip's `Nauczyciel:</b> Old -> New` line, both sides.
fn tooltip_teachers(title: &str) -> Vec<String> {
    const KEY: &str = "Nauczyciel:</b>";
    let Some(i) = title.find(KEY) else {
        return Vec::new();
    };
    let rest = &title[i + KEY.len()..];
    let line = rest.split("<br").next().unwrap_or(rest);
    line.split("->")
        .map(squash)
        .filter(|n| !n.is_empty())
        .collect()
}

/// Every distinct teacher name on a page, in document order: lesson cells (struck
/// included) and tooltips (`title` attributes), both sides of a substitution.
/// Best effort: a page that isn't a timetable yields nothing.
pub fn teacher_names(html: &str) -> Vec<String> {
    let doc = Html::parse_document(html);
    let (row, text) = (sel("table.plan-lekcji tr.line1"), sel(".text"));
    let mut names: Vec<String> = Vec::new();
    let mut add = |n: String| {
        if !names.contains(&n) {
            names.push(n);
        }
    };
    for tr in doc.select(&row) {
        for el in tr.descendants().filter_map(ElementRef::wrap) {
            if text.matches(&el)
                && let Some(n) = parse_text(el).1
            {
                add(n);
            }
            if let Some(title) = el.value().attr("title") {
                tooltip_teachers(title).into_iter().for_each(&mut add);
            }
        }
    }
    names
}

/// Logs in and fetches the current and the next week, merged. A page that isn't
/// a timetable is an error, never an empty week.
pub async fn fetch_two_weeks(
    client: &LibrusClient,
    login: &str,
    password: &str,
) -> Result<Timetable, SourceError> {
    client.login(login, password, None).await?;
    let current = client.timetable_page(None).await?;
    let (monday, mut days) = parse_week(&current)?;
    let options = week_options(&current);
    let next = options
        .iter()
        .position(|(_, selected)| *selected)
        .and_then(|i| options.get(i + 1))
        .ok_or_else(|| SourceError::Parse("no week option after the selected one"))?;
    let html = client.timetable_page(Some(&next.0)).await?;
    let (next_monday, next_days) = parse_week(&html)?;
    // If Librus ignored the selector it sends back another week; merging that
    // would overwrite the good cache with one week missing.
    let selected = week_options(&html)
        .into_iter()
        .find(|(_, selected)| *selected)
        .map(|(v, _)| v);
    if selected.as_deref() != Some(next.0.as_str()) || next_monday == monday {
        return Err(SourceError::Parse(
            "Librus returned a different week than requested",
        ));
    }
    days.extend(next_days);
    Ok(Timetable {
        weeks: vec![monday, next_monday],
        days,
    })
}

/// The `<select name="tydzien">` options of a timetable page, in page order:
/// `(value, selected)`. Values look like `YYYY-MM-DD_YYYY-MM-DD`.
pub fn week_options(html: &str) -> Vec<(String, bool)> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse(r#"select[name="tydzien"] option"#).expect("valid selector");
    doc.select(&sel)
        .filter_map(|opt| {
            let value = opt.value().attr("value")?;
            Some((value.to_owned(), opt.value().attr("selected").is_some()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::librus::auth::tests::Hosts;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, Request, ResponseTemplate};

    const WEEK: &str = include_str!("../../../tests/fixtures/librus/handmade/week.html");

    fn d(m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, m, day).unwrap()
    }
    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn parses_handmade_week() {
        let (monday, days) = parse_week(WEEK).unwrap();
        assert_eq!(monday, d(11, 23));
        assert_eq!(days.len(), 5);
        assert_eq!(days[&d(11, 23)].len(), 2);
        assert_eq!(days[&d(11, 24)].len(), 2);
        assert_eq!(days[&d(11, 25)].len(), 1);
        assert_eq!(days[&d(11, 26)].len(), 1);
        assert_eq!(days[&d(11, 27)].len(), 1);

        let first = &days[&d(11, 23)][0];
        assert_eq!(
            *first,
            Lesson {
                start: t(8, 0),
                end: t(8, 45),
                subject: "Alchemia".into(),
                teacher: Some("Jan Testowy".into()),
                room: Some("101".into()),
                cancelled: false,
                flag: None,
            }
        );
        // Leading dash, two-digit hour.
        let second = &days[&d(11, 23)][1];
        assert_eq!((second.start, second.end), (t(8, 55), t(9, 40)));
        assert_eq!(second.teacher.as_deref(), Some("Adam Przykladowy"));
        assert_eq!(second.room.as_deref(), Some("12"));
        // nbsp in the room.
        assert_eq!(days[&d(11, 25)][0].room.as_deref(), Some("Sala gimn."));
        // Cancelled.
        let cancelled = &days[&d(11, 26)][0];
        assert!(cancelled.cancelled);
        assert_eq!(cancelled.flag.as_deref(), Some("odwołane"));
        // Substitution: the active entry wins.
        let sub = &days[&d(11, 24)][0];
        assert!(!sub.cancelled);
        assert_eq!(sub.flag.as_deref(), Some("zastępstwo"));
        assert_eq!(sub.teacher.as_deref(), Some("Ola Zastepcza"));
        assert_eq!(sub.room.as_deref(), Some("102"));
        // No room.
        let friday = &days[&d(11, 27)][0];
        assert_eq!(friday.teacher.as_deref(), Some("Adam Przykladowy"));
        assert_eq!(friday.room, None);
        // Break rows are skipped: 3 lesson rows only.
        assert_eq!(days.values().map(Vec::len).sum::<usize>(), 7);
    }

    #[test]
    fn group_cell_yields_one_lesson_per_active_entry() {
        let html = r#"<select name="tydzien"><option value="2026-11-23_2026-11-29" selected>x</option></select>
<table class="decorated plan-lekcji"><tr class="line1"><td>1</td><th>8:00 - 8:45</th>
<td><div class="center"><div class="text"><b>Angielski gr. 1</b><br/>Ala Grupowa s. 3</div>
<div class="text"><b>Angielski gr. 2</b><br/>Ola Grupowa s. 4</div>
<s><div class="text"><b>Stary</b><br/>Ela Skreslona s. 5</div></s>
<div class="plan-lekcji-info">zastępstwo</div></div></td><td></td><td>1</td></tr></table>"#;
        let (_, days) = parse_week(html).unwrap();
        let day = &days[&d(11, 23)];
        assert_eq!(day.len(), 2);
        assert_eq!(day[0].subject, "Angielski gr. 1");
        assert_eq!(day[1].subject, "Angielski gr. 2");
        assert!(day.iter().all(|l| l.start == t(8, 0) && l.end == t(8, 45)));
        assert!(day.iter().all(|l| !l.cancelled));
        assert!(day.iter().all(|l| l.flag.as_deref() == Some("zastępstwo")));
        assert_eq!(
            teacher_names(html),
            vec!["Ala Grupowa", "Ola Grupowa", "Ela Skreslona"]
        );
    }

    #[test]
    fn parses_real_markup_shapes() {
        let html = r#"<select name="tydzien"><option value="2026-11-23_2026-11-29" selected>x</option></select>
<table class="decorated plan-lekcji"><tr class="line1"><td>1</td><th class="center border-top" NOWRAP>07:50&nbsp;-&nbsp;08:35</th>
<td class="line1"><div class="text">
   <b>Język polski</b>
   <br/>-Jan Przykladowy&nbsp&nbsp;s.&nbsp;10</div></td>
<td class="line1"><div class="text"><b>Język angielski</b><br/>- Ewa Testowa&nbsp;(5c grupa 1)&nbsp&nbsp;s.&nbsp;9</div></td>
<td class="line1"><s><s><div class="text"><b>Wiedza</b><br/>- Adam Probny</div></s></s></td>
<td></td><td>1</td></tr></table>"#;
        let (_, days) = parse_week(html).unwrap();
        let a = &days[&d(11, 23)][0];
        assert_eq!(a.subject, "Język polski");
        assert_eq!(a.teacher.as_deref(), Some("Jan Przykladowy"));
        assert_eq!(a.room.as_deref(), Some("10"));
        assert_eq!((a.start, a.end), (t(7, 50), t(8, 35)));
        let b = &days[&d(11, 24)][0];
        assert_eq!(b.teacher.as_deref(), Some("Ewa Testowa"));
        assert_eq!(b.room.as_deref(), Some("9"));
        let c = &days[&d(11, 25)][0];
        assert_eq!(c.teacher.as_deref(), Some("Adam Probny"));
        assert_eq!(c.room, None);
        assert!(c.cancelled);
    }

    #[test]
    fn teacher_names_include_both_sides_of_tooltips() {
        let html = r#"<table class="decorated plan-lekcji"><tr class="line1"><td>1</td><th>8:00 - 8:45</th>
<td><a href="javascript:void(0);" title="<b>Data:</b> 2026-11-23<br><b>Nauczyciel:</b> Przykladowy Jan -> Testowa Ewa<br><b>Przedmiot:</b> A -> B<br><b>Klasa:</b> 5c -> 5C"></a>
<div class="text"><b>B</b><br/>- Ewa Testowa&nbsp&nbsp;s.&nbsp;3</div></td><td>1</td></tr></table>"#;
        assert_eq!(
            teacher_names(html),
            vec!["Przykladowy Jan", "Testowa Ewa", "Ewa Testowa"]
        );
    }

    #[test]
    fn parse_week_rejects_login_page() {
        let err = parse_week("<html><body><form>Zaloguj</form></body></html>").unwrap_err();
        assert!(matches!(err, SourceError::NotTimetable), "{err:?}");
    }

    #[test]
    fn empty_week_is_ok_and_empty() {
        let html = r#"<select name="tydzien"><option value="2026-12-21_2026-12-27" selected>x</option></select>
<table class="decorated plan-lekcji"><tbody></tbody></table>"#;
        let (monday, days) = parse_week(html).unwrap();
        assert_eq!(monday, d(12, 21));
        assert!(days.is_empty());
    }

    /// Files under `tests/fixtures/librus/kid*/` (or `$LIBRUS_FIXTURES_DIR`) whose
    /// names match `keep`.
    fn owner_files(keep: impl Fn(&str) -> bool) -> Vec<std::path::PathBuf> {
        let root = std::env::var_os("LIBRUS_FIXTURES_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/librus")
            });
        let Ok(dirs) = std::fs::read_dir(&root) else {
            eprintln!("skipping: {} is missing", root.display());
            return Vec::new();
        };
        let mut out = Vec::new();
        for dir in dirs.flatten() {
            if !dir.file_name().to_string_lossy().starts_with("kid") {
                continue;
            }
            for f in std::fs::read_dir(dir.path()).unwrap().flatten() {
                if keep(&f.file_name().to_string_lossy()) {
                    out.push(f.path());
                }
            }
        }
        out.sort();
        if out.is_empty() {
            eprintln!("skipping: no owner fixtures");
        }
        out
    }

    #[test]
    fn owner_fixtures_parse() {
        let files = owner_files(|n| n.starts_with("timetable-") && n.ends_with(".html"));
        for f in files {
            let html = std::fs::read_to_string(&f).unwrap();
            let (_, days) = parse_week(&html).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
            let lessons: Vec<_> = days.values().flatten().collect();
            assert!(!lessons.is_empty(), "{}: no lessons", f.display());
            for l in lessons {
                assert!(l.start < l.end, "{}: {l:?}", f.display());
                assert!(!l.subject.is_empty(), "{}: {l:?}", f.display());
                for field in [Some(&l.subject), l.teacher.as_ref(), l.room.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    assert!(
                        !field.contains("&nbsp")
                            && !field.contains('\u{a0}')
                            && !field.starts_with('-'),
                        "{}: dirty field {field:?} in {l:?}",
                        f.display()
                    );
                }
            }
        }
    }

    #[test]
    fn owner_fixtures_are_scrubbed() {
        use crate::sources::librus::capture::{redact_session_secrets, scrub_school_codes};
        for f in owner_files(|n| n.ends_with(".html") || n.ends_with(".txt")) {
            let text = std::fs::read_to_string(&f).unwrap();
            // The scrubber leaves nothing to change.
            assert!(
                scrub_school_codes(&text) == text,
                "{}: unscrubbed school code or name",
                f.display()
            );
            assert!(
                redact_session_secrets(&text) == text,
                "{}: unredacted session secret (OAuth or CSRF)",
                f.display()
            );
            if f.extension().is_some_and(|e| e == "html") {
                let names = teacher_names(&text);
                assert!(!names.is_empty(), "{}: no teachers found", f.display());
                for n in names {
                    let ok = n
                        .strip_prefix("Nauczyciel ")
                        .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()));
                    assert!(ok, "{}: unscrubbed teacher {n:?}", f.display());
                }
            }
        }
    }

    const PAGE: &str = r#"<html><body><form method="post">
<select name="tydzien" onchange="this.form.submit();">
  <option value="2026-11-16_2026-11-22">16.11.2026 - 22.11.2026</option>
  <option value="2026-11-23_2026-11-29" selected="selected">23.11.2026 - 29.11.2026</option>
  <option value="2026-11-30_2026-12-06">30.11.2026 - 06.12.2026</option>
</select>
<select name="inny"><option value="x" selected>x</option></select>
</form></body></html>"#;

    #[test]
    fn week_options_finds_selected_and_next() {
        let opts = week_options(PAGE);
        assert_eq!(opts.len(), 3);
        let sel = opts.iter().position(|(_, s)| *s).unwrap();
        assert_eq!(opts[sel].0, "2026-11-23_2026-11-29");
        assert_eq!(opts[sel + 1], ("2026-11-30_2026-12-06".to_owned(), false));
        assert_eq!(opts.iter().filter(|(_, s)| *s).count(), 1);
    }

    #[test]
    fn week_options_empty_without_select() {
        assert!(week_options("<html><body>login</body></html>").is_empty());
    }

    async fn mount_login(h: &Hosts) {
        h.mount_entry().await;
        h.mount_post_redirect("/OAuth/Authorization/2FA?client_id=46")
            .await;
        h.mount_tail(ResponseTemplate::new(302).insert_header("Location", "/rodzic/index"))
            .await;
    }

    async fn mount_current(h: &Hosts, body: String) {
        Mock::given(method("GET"))
            .and(path("/przegladaj_plan_lekcji"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&h.synergia)
            .await;
    }

    fn next_week_page() -> String {
        WEEK.replace(
            "2026-11-23_2026-11-29\" selected=\"selected\"",
            "2026-11-23_2026-11-29\"",
        )
        .replace(
            "2026-11-30_2026-12-06\"",
            "2026-11-30_2026-12-06\" selected=\"selected\"",
        )
    }

    #[tokio::test]
    async fn fetch_two_weeks_merges() {
        let h = Hosts::start().await;
        mount_login(&h).await;
        mount_current(&h, WEEK.to_owned()).await;
        Mock::given(method("POST"))
            .and(path("/przegladaj_plan_lekcji"))
            .and(|req: &Request| {
                String::from_utf8_lossy(&req.body).contains("2026-11-30_2026-12-06")
            })
            .respond_with(ResponseTemplate::new(200).set_body_string(next_week_page()))
            .expect(1)
            .mount(&h.synergia)
            .await;

        let client = LibrusClient::new(h.urls());
        let tt = fetch_two_weeks(&client, "login", "pw").await.unwrap();
        assert_eq!(tt.weeks, vec![d(11, 23), d(11, 30)]);
        // The next page is the same fixture shifted by a week.
        assert_eq!(tt.days.len(), 10);
        assert!(tt.days.contains_key(&d(12, 4)));
    }

    #[tokio::test]
    async fn fetch_two_weeks_rejects_non_timetable_pages() {
        // Current week is not a timetable.
        let h = Hosts::start().await;
        mount_login(&h).await;
        mount_current(&h, "<html>Zaloguj</html>".into()).await;
        let client = LibrusClient::new(h.urls());
        let err = fetch_two_weeks(&client, "l", "p").await.unwrap_err();
        assert!(matches!(err, SourceError::NotTimetable), "{err:?}");

        // Next week is not a timetable.
        let h = Hosts::start().await;
        mount_login(&h).await;
        mount_current(&h, WEEK.to_owned()).await;
        Mock::given(method("POST"))
            .and(path("/przegladaj_plan_lekcji"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>Zaloguj</html>"))
            .mount(&h.synergia)
            .await;
        let client = LibrusClient::new(h.urls());
        let err = fetch_two_weeks(&client, "l", "p").await.unwrap_err();
        assert!(matches!(err, SourceError::NotTimetable), "{err:?}");
    }

    #[tokio::test]
    async fn fetch_two_weeks_rejects_the_wrong_week() {
        let h = Hosts::start().await;
        mount_login(&h).await;
        mount_current(&h, WEEK.to_owned()).await;
        // The POST is ignored and the current week comes back.
        Mock::given(method("POST"))
            .and(path("/przegladaj_plan_lekcji"))
            .respond_with(ResponseTemplate::new(200).set_body_string(WEEK))
            .mount(&h.synergia)
            .await;
        let client = LibrusClient::new(h.urls());
        let err = fetch_two_weeks(&client, "l", "p").await.unwrap_err();
        assert!(matches!(err, SourceError::Parse(_)), "{err:?}");
        assert!(
            err.to_string()
                .contains("Librus returned a different week than requested"),
            "{err}"
        );
        assert!(!err.to_string().contains("2026-11-30"), "{err}");
    }
}
