//! `tablica capture librus`: record the login chain and timetable pages as fixtures.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Context;

use super::auth::{Exchange, LibrusClient, LibrusUrls};
use super::timetable::{teacher_names, week_options};
use crate::family::config::{FamilyConfig, KidConfig};

const REDACTED: &str = "<redacted>";
/// Cookie values shorter than this are only redacted in `Set-Cookie` headers, not
/// searched for elsewhere, so values like `1` don't mangle the files.
const MIN_COOKIE_SCRUB_LEN: usize = 8;

/// Captures fixtures from production Librus for every configured kid.
pub async fn capture(family: &FamilyConfig, out: &Path) -> anyhow::Result<()> {
    capture_with_urls(family, out, &LibrusUrls::default()).await
}

/// Like [`capture`], against the given hosts.
///
/// For kid N it writes `kid{N}/NN-<method>-<host>.txt` for every login response and
/// `kid{N}/timetable-current.html` / `timetable-next.html`. The login is replaced
/// with `LOGIN`, cookie values with `<redacted>`, and the password (never sent back
/// by Librus, but checked anyway) with `<redacted>`.
pub async fn capture_with_urls(
    family: &FamilyConfig,
    out: &Path,
    urls: &LibrusUrls,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !family.kids.is_empty(),
        "no [[family.kid]] entries in the config"
    );
    for (i, kid) in family.kids.iter().enumerate() {
        let n = i + 1;
        let dir = out.join(format!("kid{n}"));
        capture_kid(kid, &dir, urls)
            .await
            .with_context(|| format!("kid {n}"))?;
        println!("kid {n}: wrote {}", dir.display());
    }
    println!(
        "\nReview every file under {} before committing it. The login, password, cookies, \
         session tokens, teacher names and school codes are scrubbed; kids' names, rooms \
         and the timetable itself are not.",
        out.display()
    );
    Ok(())
}

async fn capture_kid(kid: &KidConfig, dir: &Path, urls: &LibrusUrls) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let client = LibrusClient::new(urls.clone());
    let mut trace = Vec::new();
    let login = client
        .login(&kid.librus_login, &kid.librus_password, Some(&mut trace))
        .await;

    // Write the trace before checking the result: a failed chain is what most
    // needs looking at.
    let mut scrub = Scrubber::new(kid);
    for ex in &trace {
        scrub.learn_cookies(ex);
    }
    for (j, ex) in trace.iter().enumerate() {
        let host = reqwest::Url::parse(&ex.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".to_owned());
        let name = format!("{:02}-{}-{host}.txt", j + 1, ex.method);
        write(&dir.join(name), &scrub.exchange(ex))?;
    }
    login.with_context(|| format!("login failed; trace written to {}", dir.display()))?;

    // Both pages stay in memory until every teacher on them is known, so no
    // file with a real name in it is ever written.
    let current = client
        .timetable_page(None)
        .await
        .context("fetching the current week")?;
    let current = scrub.text(&current);
    let mut teachers = Vec::new();
    add_teachers(&current, &mut teachers);
    let options = week_options(&current);
    let next_week = options
        .iter()
        .position(|(_, selected)| *selected)
        .and_then(|i| options.get(i + 1));
    let mut pages = vec![("timetable-current.html", current.clone())];
    let mut failure = None;
    match next_week {
        Some((week, _)) => match client.timetable_page(Some(week)).await {
            Ok(html) => {
                let html = scrub.text(&html);
                add_teachers(&html, &mut teachers);
                pages.push(("timetable-next.html", html));
            }
            Err(e) => {
                failure = Some(anyhow::Error::new(e).context(format!("fetching week {week}")));
            }
        },
        None => eprintln!(
            "warning: {}: no week option after the selected one; skipped the next week",
            dir.display()
        ),
    }
    let labels = teacher_labels(&teachers);
    for (name, html) in &pages {
        write(&dir.join(name), &relabel(html, &labels))?;
    }
    // The trace files are already on disk; names can appear there too.
    scrub_teachers(dir, &labels)?;
    failure.map_or(Ok(()), Err)
}

/// Adds the teacher names on a timetable page (struck lessons and tooltips too),
/// first seen first. Already-scrubbed `Nauczyciel N` values are skipped.
fn add_teachers(html: &str, names: &mut Vec<String>) {
    for name in teacher_names(html) {
        let scrubbed = name
            .strip_prefix("Nauczyciel ")
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if !scrubbed && !names.contains(&name) {
            names.push(name);
        }
    }
}

/// "Surname Firstname" and "Firstname Surname" are the same teacher.
fn teacher_key(name: &str) -> Vec<String> {
    let mut words: Vec<String> = name.split_whitespace().map(str::to_lowercase).collect();
    words.sort();
    words
}

/// `(name, "Nauczyciel N")` for every teacher (N from 1 in order of first
/// appearance; word order doesn't matter), longest name first so a name
/// containing another isn't left half replaced.
fn teacher_labels(names: &[String]) -> Vec<(String, String)> {
    let mut keys: Vec<Vec<String>> = Vec::new();
    let mut labelled: Vec<(String, String)> = Vec::new();
    for name in names {
        let key = teacher_key(name);
        let n = keys.iter().position(|k| *k == key).unwrap_or_else(|| {
            keys.push(key);
            keys.len() - 1
        });
        labelled.push((name.clone(), format!("Nauczyciel {}", n + 1)));
    }
    labelled.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
    labelled
}

/// Replaces every teacher name in `text` with its label.
fn relabel(text: &str, labels: &[(String, String)]) -> String {
    let mut text = text.to_owned();
    for (name, label) in labels {
        text = text.replace(name.as_str(), label);
        // The page may spell the gaps as `&nbsp;`.
        text = text.replace(&name.replace(' ', "&nbsp;"), label);
    }
    text
}

/// Relabels teacher names in every file under `dir`.
fn scrub_teachers(dir: &Path, labels: &[(String, String)]) -> anyhow::Result<()> {
    if labels.is_empty() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        write(&path, &relabel(&text, labels))?;
    }
    Ok(())
}

fn write(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

/// Replaces secrets in everything capture writes.
struct Scrubber {
    /// `(needle, replacement)`, applied in order.
    replacements: Vec<(String, &'static str)>,
}

impl Scrubber {
    fn new(kid: &KidConfig) -> Self {
        let mut s = Self {
            replacements: Vec::new(),
        };
        s.add(&kid.librus_password, REDACTED);
        s.add(&kid.librus_login, "LOGIN");
        s
    }

    /// Adds `value` and its percent-encoded form.
    fn add(&mut self, value: &str, replacement: &'static str) {
        if value.is_empty() {
            return;
        }
        for needle in [value.to_owned(), percent_encode(value)] {
            if !self.replacements.iter().any(|(n, _)| *n == needle) {
                self.replacements.push((needle, replacement));
            }
        }
    }

    /// Remembers cookie values set in `ex`, so they're scrubbed wherever they appear.
    fn learn_cookies(&mut self, ex: &Exchange) {
        for (name, value) in &ex.headers {
            if name.eq_ignore_ascii_case("set-cookie")
                && let Some((_, v)) = cookie_pair(value)
                && v.len() >= MIN_COOKIE_SCRUB_LEN
            {
                // Cookie values go before the login so a login inside one can't
                // leave a partial value behind.
                let v = v.to_owned();
                self.replacements.insert(0, (v, REDACTED));
            }
        }
    }

    fn text(&self, s: &str) -> String {
        let s = self
            .replacements
            .iter()
            .fold(s.to_owned(), |acc, (needle, rep)| acc.replace(needle, rep));
        scrub_school_codes(&redact_session_secrets(&s))
    }

    fn exchange(&self, ex: &Exchange) -> String {
        let mut out = format!("{} {}\nstatus: {}\n\n", ex.method, ex.url, ex.status);
        for (name, value) in &ex.headers {
            let value = if name.eq_ignore_ascii_case("set-cookie") {
                redact_set_cookie(value)
            } else {
                value.clone()
            };
            let _ = writeln!(out, "{name}: {value}");
        }
        out.push('\n');
        out.push_str(&ex.body);
        self.text(&out)
    }
}

/// School identifier prefixes, longest first.
const SCHOOL_PREFIXES: [&str; 9] = ["CKZiU", "ZSO", "ZSP", "XLO", "SP", "LO", "ZS", "PG", "G"];

const SCHOOL_NAME_SCRUBBED: &str = "Szkoła Podstawowa nr 000";
const SCHOOL_NAME_WORDS: [&str; 3] = ["szkoła", "podstawowa", "nr"];

/// Byte length of a case-insensitive `Szkoła Podstawowa nr <digits>` (`ł` may be
/// spelled `l`) at the start of `text`, if there is one.
fn school_name_len(text: &str) -> Option<usize> {
    let mut chars = text.char_indices().peekable();
    for (w, word) in SCHOOL_NAME_WORDS.iter().enumerate() {
        for want in word.chars() {
            let (_, c) = chars.next()?;
            let c = c.to_lowercase().next()?;
            if !(c == want || (want == 'ł' && c == 'l')) {
                return None;
            }
        }
        let (_, sp) = chars.next()?;
        if w < 2 && sp != ' ' || w == 2 && sp != ' ' {
            return None;
        }
    }
    let mut end = None;
    while let Some(&(i, c)) = chars.peek() {
        if !c.is_ascii_digit() {
            break;
        }
        end = Some(i + 1);
        chars.next();
    }
    let end = end?;
    (!text[end..]
        .chars()
        .next()
        .is_some_and(char::is_alphanumeric))
    .then_some(end)
}

/// Replaces school codes such as `SP123` or `LO 5` with the prefix plus `000`.
/// A code is a prefix at a word start, an optional space (not for a bare `G`) and
/// digits that end at a word boundary.
pub(crate) fn scrub_school_codes(text: &str) -> String {
    scrub_class_labels(&scrub_school_ids(text))
}

fn scrub_school_ids(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    'outer: while i < text.len() {
        let rest = &text[i..];
        let word_start = !text[..i]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric);
        if word_start && let Some(len) = school_name_len(rest) {
            out.push_str(SCHOOL_NAME_SCRUBBED);
            i += len;
            continue;
        }
        if word_start {
            for p in SCHOOL_PREFIXES {
                let Some(after) = rest.strip_prefix(p) else {
                    continue;
                };
                let after = match after.strip_prefix(' ') {
                    Some(a) if p.len() > 1 => a,
                    _ => after,
                };
                let digits = after.chars().take_while(char::is_ascii_digit).count();
                let next = after[digits..].chars().next();
                if digits > 0 && !next.is_some_and(char::is_alphanumeric) {
                    out.push_str(p);
                    out.push_str("000");
                    i = text.len() - after.len() + digits;
                    continue 'outer;
                }
            }
        }
        let c = rest.chars().next().expect("non-empty");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Whether the text before the cursor is inside a `Klasa:</b>` tooltip line.
fn in_class_line(before: &str) -> bool {
    let line_start = before.rfind("<br").map_or(0, |i| i + 3);
    before[line_start..].contains("Klasa:</b>")
}

/// Replaces class labels (a digit 1-8 and one letter, as a whole word) with `0x`,
/// keeping the letter's case. Only in the shapes Librus puts them in: after
/// `Klasa:</b> `, after `-> ` inside a `Klasa:</b>` line (up to the next `<br`), and in `(<class> grupa …)`. Other tokens such as
/// room numbers (`1A`) are left alone.
fn scrub_class_labels(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let before = &text[..i];
        let rest = &text[i..];
        let mut chars = rest.chars();
        if let (Some(d), Some(l)) = (chars.next(), chars.next())
            && ('1'..='8').contains(&d)
            && l.is_ascii_alphabetic()
        {
            let after = &rest[2..];
            let whole = !after.chars().next().is_some_and(char::is_alphanumeric);
            let context = before.ends_with("Klasa:</b> ")
                || (before.ends_with("-> ") && in_class_line(before))
                || (before.ends_with('(') && after.starts_with(" grupa"));
            if whole && context {
                out.push('0');
                out.push(if l.is_ascii_uppercase() { 'X' } else { 'x' });
                i += 2;
                continue;
            }
        }
        let c = rest.chars().next().expect("non-empty");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// `name` and `value` of a `Set-Cookie` header.
fn cookie_pair(header: &str) -> Option<(&str, &str)> {
    let pair = header.split(';').next()?;
    let (name, value) = pair.split_once('=')?;
    Some((name.trim(), value.trim()))
}

/// `name=value; attrs` → `name=<redacted>; attrs`.
fn redact_set_cookie(header: &str) -> String {
    let (pair, attrs) = match header.split_once(';') {
        Some((p, a)) => (p, Some(a)),
        None => (header, None),
    };
    let name = pair.split_once('=').map_or(pair, |(n, _)| n).trim();
    match attrs {
        Some(a) => format!("{name}={REDACTED};{a}"),
        None => format!("{name}={REDACTED}"),
    }
}

/// OAuth parameters whose values never reach a fixture.
/// Query parameters and `<input>` names whose values are secrets: the OAuth
/// `code`/`state` and Synergia's CSRF `requestkey`.
const SECRET_PARAMS: [&str; 3] = ["code", "state", "requestkey"];

/// Replaces OAuth `code` and `state` values with `<redacted>` anywhere in `text`:
/// URL query parameters (after `?`, `&`, `&amp;` or JSON-escaped `\u0026`) and
/// the `value` of `<input name="code|state">` fields.
fn redact_oauth_params(text: &str) -> String {
    redact_oauth_inputs(&redact_oauth_query(text))
}

/// Every session secret capture knows how to find in page text: OAuth
/// parameters, the CSRF `requestkey` input and the page's `csrfTokenValue`.
/// Applying it to already-redacted text changes nothing.
pub(crate) fn redact_session_secrets(text: &str) -> String {
    redact_csrf_js(&redact_oauth_params(text))
}

/// Redacts the string assigned to `csrfTokenValue` in the page's script.
fn redact_csrf_js(text: &str) -> String {
    const NAME: &str = "csrfTokenValue";
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(off) = text[i..].find(NAME) {
        let after = i + off + NAME.len();
        out.push_str(&text[i..after]);
        i = after;
        let rest = &text[after..];
        let eq = rest.len() - rest.trim_start().len();
        if !rest[eq..].starts_with('=') {
            continue;
        }
        let rest2 = &rest[eq + 1..];
        let q_off = eq + 1 + (rest2.len() - rest2.trim_start().len());
        let Some(quote) = rest[q_off..]
            .chars()
            .next()
            .filter(|c| matches!(c, '"' | '\''))
        else {
            continue;
        };
        let body_start = after + q_off + 1;
        let Some(len) = text[body_start..].find(quote) else {
            continue;
        };
        out.push_str(&text[after..body_start]);
        out.push_str(REDACTED);
        i = body_start + len;
    }
    out.push_str(&text[i..]);
    out
}

fn redact_oauth_query(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let param = SECRET_PARAMS.iter().find(|p| {
            rest.strip_prefix(**p).is_some_and(|r| r.starts_with('='))
                && (i == 0 || {
                    let before = &text[..i];
                    before.ends_with(['?', '&', ';']) || before.ends_with("\\u0026")
                })
        });
        match param {
            Some(p) if text[i + p.len() + 1..].starts_with(REDACTED) => {
                // Already redacted: copy it as is.
                let end = i + p.len() + 1 + REDACTED.len();
                out.push_str(&text[i..end]);
                i = end;
            }
            Some(p) => {
                let value_start = i + p.len() + 1;
                let value_len = text[value_start..]
                    .find(|c: char| {
                        matches!(c, '&' | '"' | '\'' | '#' | '<' | '\\') || c.is_whitespace()
                    })
                    .unwrap_or(text.len() - value_start);
                out.push_str(&text[i..value_start]);
                out.push_str(REDACTED);
                i = value_start + value_len;
            }
            None => {
                let c = rest.chars().next().expect("non-empty");
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    out
}

/// Redacts the `value` attribute of `<input>` tags named `code` or `state`.
fn redact_oauth_inputs(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(off) = lower[i..].find("<input") {
        let start = i + off;
        let end = lower[start..].find('>').map_or(text.len(), |e| start + e);
        out.push_str(&text[i..start]);
        out.push_str(&redact_input_tag(&text[start..end], &lower[start..end]));
        i = end;
    }
    out.push_str(&text[i..]);
    out
}

fn redact_input_tag(tag: &str, lower: &str) -> String {
    let named = SECRET_PARAMS.iter().any(|p| {
        lower.contains(&format!("name=\"{p}\"")) || lower.contains(&format!("name='{p}'"))
    });
    // `value=` as its own attribute, not the tail of e.g. `data-value=`.
    let Some(v) = lower
        .match_indices("value=")
        .map(|(i, _)| i)
        .find(|&i| lower[..i].ends_with(char::is_whitespace))
    else {
        return tag.to_owned();
    };
    if !named {
        return tag.to_owned();
    }
    let value_start = v + "value=".len();
    let (open, close): (usize, &[char]) = match tag[value_start..].chars().next() {
        Some('"') => (1, &['"']),
        Some('\'') => (1, &['\'']),
        _ => (0, &[' ', '\t', '\n', '/']),
    };
    let body_start = value_start + open;
    // Already redacted (the tag was cut at the `>` inside the marker).
    if tag[body_start..].starts_with(&REDACTED[..REDACTED.len() - 1]) {
        return tag.to_owned();
    }
    let body_end = tag[body_start..]
        .find(close)
        .map_or(tag.len(), |e| body_start + e);
    format!("{}{REDACTED}{}", &tag[..body_start], &tag[body_end..])
}

/// Percent-encodes everything but unreserved characters (RFC 3986).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn scrubs_class_labels() {
        let f = super::scrub_school_codes;
        assert_eq!(
            f("<b>Klasa:</b> 5c grupa 1 -> 5C SP123"),
            "<b>Klasa:</b> 0x grupa 1 -> 0X SP000"
        );
        assert_eq!(f("Klasa:</b> 5C SP12"), "Klasa:</b> 0X SP000");
        assert_eq!(
            f("Name&nbsp;(5c grupa 1)&nbsp&nbsp;s.&nbsp;1A"),
            "Name&nbsp;(0x grupa 1)&nbsp&nbsp;s.&nbsp;1A"
        );
        // `->` outside a class line stays: rooms and subjects can look like classes.
        assert_eq!(f("<b>Sala:</b> 15 -> 1A"), "<b>Sala:</b> 15 -> 1A");
        assert_eq!(
            f("<b>Przedmiot:</b> Fizyka -> 2b"),
            "<b>Przedmiot:</b> Fizyka -> 2b"
        );
        assert_eq!(f("1A -> 2B"), "1A -> 2B");
        assert_eq!(
            f("<b>Klasa:</b> 5c -> 5C<br><b>Sala:</b> 1A -> 2B"),
            "<b>Klasa:</b> 0x -> 0X<br><b>Sala:</b> 1A -> 2B"
        );
        // Other tokens stay: rooms, other parentheses, longer words, out of range.
        assert_eq!(
            f("s.&nbsp;1A H1 (2b) 9a -> 9a x 8bc"),
            "s.&nbsp;1A H1 (2b) 9a -> 9a x 8bc"
        );
        // Idempotent.
        let once = f("Klasa:</b> 5c grupa 1 -> 5C SP1 (7c grupa 2)");
        assert_eq!(f(&once), once);
    }

    #[test]
    fn scrubs_school_codes() {
        assert_eq!(
            super::scrub_school_codes("5C SP123, <b>Klasa:</b> SP 12 -> LO7."),
            "5C SP000, <b>Klasa:</b> SP000 -> LO000."
        );
        assert_eq!(
            super::scrub_school_codes("<td>SZKOŁA PODSTAWOWA NR 5, Szkola Podstawowa nr 12.</td>"),
            "<td>Szkoła Podstawowa nr 000, Szkoła Podstawowa nr 000.</td>"
        );
        assert_eq!(
            super::scrub_school_codes("Szkoła Podstawowa nr 000"),
            "Szkoła Podstawowa nr 000"
        );
        assert_eq!(
            super::scrub_school_codes("Szkoła Podstawowa nr x"),
            "Szkoła Podstawowa nr x"
        );
        // Not codes: no digits, glued letters, inside a word, bare G with a space.
        assert_eq!(
            super::scrub_school_codes("SPORT SP1a XSP1 G 5 G5 id=1"),
            "SPORT SP1a XSP1 G 5 G000 id=1"
        );
    }

    #[test]
    fn redacts_code_and_state_query_values() {
        assert_eq!(
            super::redact_oauth_params("https://h/p?client_id=46&state=xyz&code=a1#f"),
            "https://h/p?client_id=46&state=<redacted>&code=<redacted>#f"
        );
        assert_eq!(
            super::redact_oauth_params("/p?code=a&response_type=code"),
            "/p?code=<redacted>&response_type=code"
        );
        assert_eq!(super::redact_oauth_params("/p?zipcode=1"), "/p?zipcode=1");
    }

    #[test]
    fn redacts_csrf_tokens() {
        let page = r#"<script>
            var csrfTokenName = "requestkey";
            var csrfTokenValue = "MC4xSECRETVALUE";
            var other = csrfTokenValue;
        </script>
        <form><input type="hidden" name="requestkey" value="MC4xSECRETVALUE" /></form>
        <a href="/x?requestkey=MC4xSECRETVALUE&y=1">x</a>"#;
        let out = redact_session_secrets(page);
        assert!(!out.contains("SECRETVALUE"), "{out}");
        assert!(out.contains(r#"csrfTokenValue = "<redacted>";"#), "{out}");
        assert!(
            out.contains(r#"name="requestkey" value="<redacted>""#),
            "{out}"
        );
        assert!(out.contains("var other = csrfTokenValue;"), "{out}");
        assert_eq!(redact_session_secrets(&out), out, "idempotent");
    }

    #[test]
    fn redacts_code_and_state_in_bodies() {
        assert_eq!(
            super::redact_oauth_params(r#"{"goTo":"/a?client_id=46\u0026state=s1&code=c1"}"#),
            r#"{"goTo":"/a?client_id=46\u0026state=<redacted>&code=<redacted>"}"#
        );
        assert_eq!(
            super::redact_oauth_params(r#"<a href="/x?a=1&amp;state=s2">"#),
            r#"<a href="/x?a=1&amp;state=<redacted>">"#
        );
        assert_eq!(
            super::redact_oauth_params(
                r#"<input type="hidden" name="state" value="s3"><INPUT value='c4' name='code'>"#
            ),
            r#"<input type="hidden" name="state" value="<redacted>"><INPUT value='<redacted>' name='code'>"#
        );
        assert_eq!(
            super::redact_oauth_params(r#"<input name="login" value="x">"#),
            r#"<input name="login" value="x">"#
        );
        assert_eq!(
            super::redact_oauth_params(r#"<input data-value="d" name="state" value="s5">"#),
            r#"<input data-value="d" name="state" value="<redacted>">"#
        );
    }

    use super::*;
    use crate::family::config::WeatherConfig;
    use crate::sources::librus::auth::tests::{Hosts, LOGIN, PASSWORD};
    use chrono::NaiveTime;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, Request, ResponseTemplate};

    const COOKIE_VALUE: &str = "s3ss10n-c00kie-v4lue";

    fn config() -> FamilyConfig {
        FamilyConfig {
            switch_at: NaiveTime::from_hms_opt(16, 0, 0).unwrap(),
            calendar_days: 7,
            ics_url: "http://x/cal.ics".into(),
            kids: vec![KidConfig {
                label: "Kid 1".into(),
                librus_login: LOGIN.into(),
                librus_password: PASSWORD.into(),
            }],
            weather: WeatherConfig {
                latitude: 0.0,
                longitude: 0.0,
            },
        }
    }

    fn files_under(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                out.extend(files_under(&p));
            } else {
                out.push(p);
            }
        }
        out
    }

    #[tokio::test]
    async fn capture_never_writes_password() {
        let h = Hosts::start().await;
        h.mount_entry().await;
        // The POST response sets a session cookie and echoes the login.
        Mock::given(method("POST"))
            .and(path("/OAuth/Authorization"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", "/OAuth/Authorization/2FA?client_id=46")
                    .insert_header(
                        "Set-Cookie",
                        format!("DZIENNIKSID={COOKIE_VALUE}; path=/; HttpOnly"),
                    )
                    .set_body_string(format!("{{\"user\":\"{LOGIN}\"}}")),
            )
            .mount(&h.api)
            .await;
        h.mount_tail(ResponseTemplate::new(302).insert_header("Location", "/rodzic/index"))
            .await;
        let page = |selected: usize, marker: &str| {
            let opts = ["2026-11-23_2026-11-29", "2026-11-30_2026-12-06"]
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let sel = if i == selected { " selected" } else { "" };
                    format!("<option value=\"{v}\"{sel}>{v}</option>")
                })
                .collect::<String>();
            format!(
                "<html><p>Zalogowany: {LOGIN}</p><p>{marker}</p>\
                 <select name=\"tydzien\">{opts}</select></html>"
            )
        };
        Mock::given(method("GET"))
            .and(path("/przegladaj_plan_lekcji"))
            .respond_with(ResponseTemplate::new(200).set_body_string(page(0, "CURRENT-WEEK")))
            .mount(&h.synergia)
            .await;
        Mock::given(method("POST"))
            .and(path("/przegladaj_plan_lekcji"))
            .and(|req: &Request| {
                String::from_utf8_lossy(&req.body).contains("2026-11-30_2026-12-06")
            })
            .respond_with(ResponseTemplate::new(200).set_body_string(page(1, "NEXT-WEEK")))
            .expect(1)
            .mount(&h.synergia)
            .await;

        let out = tempfile::tempdir().unwrap();
        capture_with_urls(&config(), out.path(), &h.urls())
            .await
            .unwrap();

        let kid = out.path().join("kid1");
        let current = std::fs::read_to_string(kid.join("timetable-current.html")).unwrap();
        let next = std::fs::read_to_string(kid.join("timetable-next.html")).unwrap();
        assert!(current.contains("CURRENT-WEEK"));
        assert!(next.contains("NEXT-WEEK"));
        assert!(current.contains("Zalogowany: LOGIN"), "{current}");

        let files = files_under(out.path());
        let traces: Vec<_> = files
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "txt"))
            .collect();
        assert_eq!(traces.len(), 6, "{files:?}");
        assert!(kid.join("01-GET-127.0.0.1.txt").exists(), "{files:?}");
        assert!(kid.join("04-POST-127.0.0.1.txt").exists(), "{files:?}");

        let mut saw_redacted = false;
        let mut saw_state = false;
        let mut saw_code = false;
        for f in &files {
            let text = std::fs::read_to_string(f).unwrap();
            assert!(!text.contains(PASSWORD), "password in {}", f.display());
            assert!(
                !text.contains("Sup3r-Secret"),
                "password in {}",
                f.display()
            );
            assert!(!text.contains(LOGIN), "login in {}", f.display());
            assert!(!text.contains(COOKIE_VALUE), "cookie in {}", f.display());
            assert!(!text.contains("tok-c00kie-1"), "cookie in {}", f.display());
            assert!(!text.contains("state=s1"), "oauth state in {}", f.display());
            assert!(!text.contains("code=abc"), "oauth code in {}", f.display());
            saw_redacted |= text.contains("DZIENNIKSID=<redacted>");
            saw_state |= text.contains("state=<redacted>");
            saw_code |= text.contains("code=<redacted>");
        }
        assert!(
            saw_state && saw_code,
            "code/state should be kept as <redacted>"
        );
        assert!(
            saw_redacted,
            "set-cookie name should be kept, value redacted"
        );
    }

    #[tokio::test]
    async fn capture_redacts_oauth_params_in_bodies() {
        let h = Hosts::start().await;
        h.mount_entry().await;
        // The login form carries `state` in a hidden input and in a link.
        Mock::given(method("GET"))
            .and(path("/OAuth/Authorization"))
            .and(wiremock::matchers::query_param("client_id", "46"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "<form><input type=\"hidden\" name=\"state\" value=\"SECRETSTATE3\">\
                 <input value='SECRETSTATE4' name='state'>\
                 <a href=\"/x?a=1&amp;state=SECRETSTATE5\">x</a></form>",
            ))
            .with_priority(2)
            .mount(&h.api)
            .await;
        // The POST continues through a JSON `goTo` with code and state, one `&`
        // JSON-escaped.
        Mock::given(method("POST"))
            .and(path("/OAuth/Authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"status":"ok","goTo":"/OAuth/Authorization/2FA?client_id=46\u0026state=SECRETSTATE2&code=SECRETCODE1&state=SECRETSTATE1"}"#,
            ))
            .mount(&h.api)
            .await;
        h.mount_tail(ResponseTemplate::new(302).insert_header("Location", "/rodzic/index"))
            .await;
        Mock::given(method("GET"))
            .and(path("/przegladaj_plan_lekcji"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html></html>"))
            .mount(&h.synergia)
            .await;

        let out = tempfile::tempdir().unwrap();
        capture_with_urls(&config(), out.path(), &h.urls())
            .await
            .unwrap();
        let files = files_under(out.path());
        assert_eq!(files.len(), 7, "{files:?}"); // 6 trace files + current week
        for f in &files {
            let text = std::fs::read_to_string(f).unwrap();
            assert!(
                !text.contains("SECRET"),
                "oauth value in {}:\n{text}",
                f.display()
            );
        }
    }

    #[tokio::test]
    async fn capture_writes_trace_even_when_login_fails() {
        let h = Hosts::start().await;
        h.mount_entry().await;
        h.mount_post_redirect("http://evil.example/x").await;
        let out = tempfile::tempdir().unwrap();
        let err = capture_with_urls(&config(), out.path(), &h.urls())
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("unexpected redirect"),
            "{err:#}"
        );
        assert!(out.path().join("kid1/04-POST-127.0.0.1.txt").exists());
    }

    #[tokio::test]
    async fn capture_scrubs_teacher_names() {
        let h = Hosts::start().await;
        h.mount_entry().await;
        h.mount_post_redirect("/OAuth/Authorization/2FA?client_id=46")
            .await;
        h.mount_tail(ResponseTemplate::new(302).insert_header("Location", "/rodzic/index"))
            .await;
        let week = include_str!("../../../tests/fixtures/librus/handmade/week.html");
        let next = week
            .replace(
                "2026-11-23_2026-11-29\" selected=\"selected\"",
                "2026-11-23_2026-11-29\"",
            )
            .replace(
                "2026-11-30_2026-12-06\"",
                "2026-11-30_2026-12-06\" selected=\"selected\"",
            )
            .replace("Jan Testowy", "Nowa Nauczycielka");
        // A teacher who appears only in a struck lesson.
        let week = week.replace(
            "Jan Testowy s. 101</div></s>",
            "Skreslony Nauczyciel s. 101</div></s>",
        );
        let week = week.as_str();
        Mock::given(method("GET"))
            .and(path("/przegladaj_plan_lekcji"))
            .respond_with(ResponseTemplate::new(200).set_body_string(week))
            .mount(&h.synergia)
            .await;
        Mock::given(method("POST"))
            .and(path("/przegladaj_plan_lekcji"))
            .respond_with(ResponseTemplate::new(200).set_body_string(next))
            .mount(&h.synergia)
            .await;

        let out = tempfile::tempdir().unwrap();
        capture_with_urls(&config(), out.path(), &h.urls())
            .await
            .unwrap();
        let kid = out.path().join("kid1");
        let current = std::fs::read_to_string(kid.join("timetable-current.html")).unwrap();
        let next = std::fs::read_to_string(kid.join("timetable-next.html")).unwrap();
        for name in [
            "Jan Testowy",
            "Ewa Probna",
            "Adam Przykladowy",
            "Ola Zastepcza",
            "Nowa Nauczycielka",
            "Skreslony Nauczyciel",
        ] {
            assert!(!current.contains(name) && !next.contains(name), "{name}");
        }
        // Numbered in document order of first appearance, stable across both pages.
        assert!(current.contains("<b>Alchemia</b><br/>Nauczyciel 1 s. 101"));
        assert!(current.contains("<b>Ruchy</b><br/>Nauczyciel 2 s. Sala&nbsp;gimn."));
        assert!(current.contains("- Nauczyciel 3 s. 12"));
        assert!(current.contains("Nauczyciel 4 s. 101</div></s>")); // struck-only
        assert!(current.contains("Nauczyciel 5 s. 102"));
        assert!(next.contains("Nauczyciel 2 s. 7"));
        assert!(next.contains("<br/>Nauczyciel 6 s. 101"));
    }

    #[tokio::test]
    async fn capture_scrubs_current_week_when_next_fetch_fails() {
        let h = Hosts::start().await;
        h.mount_entry().await;
        h.mount_post_redirect("/OAuth/Authorization/2FA?client_id=46")
            .await;
        h.mount_tail(ResponseTemplate::new(302).insert_header("Location", "/rodzic/index"))
            .await;
        let week = include_str!("../../../tests/fixtures/librus/handmade/week.html");
        Mock::given(method("GET"))
            .and(path("/przegladaj_plan_lekcji"))
            .respond_with(ResponseTemplate::new(200).set_body_string(week))
            .mount(&h.synergia)
            .await;
        Mock::given(method("POST"))
            .and(path("/przegladaj_plan_lekcji"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&h.synergia)
            .await;

        let out = tempfile::tempdir().unwrap();
        let err = capture_with_urls(&config(), out.path(), &h.urls())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("fetching week"), "{err:#}");
        let current =
            std::fs::read_to_string(out.path().join("kid1/timetable-current.html")).unwrap();
        assert!(!current.contains("Jan Testowy") && !current.contains("Ewa Probna"));
        assert!(current.contains("Nauczyciel 1"));
    }

    #[tokio::test]
    async fn capture_scrubs_tooltip_teachers_and_school_codes() {
        let h = Hosts::start().await;
        h.mount_entry().await;
        h.mount_post_redirect("/OAuth/Authorization/2FA?client_id=46")
            .await;
        h.mount_tail(ResponseTemplate::new(302).insert_header("Location", "/rodzic/index"))
            .await;
        let page = r#"<select name="tydzien"><option value="2026-11-23_2026-11-29" selected>x</option></select>
<table class="decorated plan-lekcji"><tr class="line1"><td>1</td><th>8:00 - 8:45</th>
<td><a href="javascript:void(0);" title="<b>Klasa:</b> 5c grupa 1 -> 5C SP123<br><b>Nauczyciel:</b> Przykladowy Jan -> Testowa Ewa<br>"></a>
<div class="text"><b>X</b><br/>- Ewa Testowa&nbsp&nbsp;s.&nbsp;3</div></td><td>1</td></tr></table>"#;
        Mock::given(method("GET"))
            .and(path("/przegladaj_plan_lekcji"))
            .respond_with(ResponseTemplate::new(200).set_body_string(page))
            .mount(&h.synergia)
            .await;
        let out = tempfile::tempdir().unwrap();
        capture_with_urls(&config(), out.path(), &h.urls())
            .await
            .unwrap();
        let cur = std::fs::read_to_string(out.path().join("kid1/timetable-current.html")).unwrap();
        assert!(
            !cur.contains("Przykladowy") && !cur.contains("Testowa"),
            "{cur}"
        );
        assert!(!cur.contains("SP123") && cur.contains("0X SP000"), "{cur}");
        // Both name orders are one teacher.
        assert!(
            cur.contains("Nauczyciel:</b> Nauczyciel 1 -> Nauczyciel 2"),
            "{cur}"
        );
        assert!(
            cur.contains("Nauczyciel 2 s. 3") || cur.contains("Nauczyciel 2&nbsp&nbsp;s."),
            "{cur}"
        );
    }
}
