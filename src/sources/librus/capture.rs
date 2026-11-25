//! `tablica capture librus`: record the login chain and timetable pages as fixtures.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Context;

use super::auth::{Exchange, LibrusClient, LibrusUrls};
use super::timetable::week_options;
use crate::config::{Config, KidConfig};

const REDACTED: &str = "<redacted>";
/// Cookie values shorter than this are only redacted in `Set-Cookie` headers, not
/// searched for elsewhere, so values like `1` don't mangle the files.
const MIN_COOKIE_SCRUB_LEN: usize = 8;

/// Captures fixtures from production Librus for every configured kid.
pub async fn capture(cfg: &Config, out: &Path) -> anyhow::Result<()> {
    capture_with_urls(cfg, out, &LibrusUrls::default()).await
}

/// Like [`capture`], against the given hosts.
///
/// For kid N it writes `kid{N}/NN-<method>-<host>.txt` for every login response and
/// `kid{N}/timetable-current.html` / `timetable-next.html`. The login is replaced
/// with `LOGIN`, cookie values with `<redacted>`, and the password (never sent back
/// by Librus, but checked anyway) with `<redacted>`.
pub async fn capture_with_urls(cfg: &Config, out: &Path, urls: &LibrusUrls) -> anyhow::Result<()> {
    anyhow::ensure!(
        !cfg.family.kids.is_empty(),
        "no [[family.kid]] entries in the config"
    );
    for (i, kid) in cfg.family.kids.iter().enumerate() {
        let n = i + 1;
        let dir = out.join(format!("kid{n}"));
        capture_kid(kid, &dir, urls)
            .await
            .with_context(|| format!("kid {n}"))?;
        println!("kid {n}: wrote {}", dir.display());
    }
    println!(
        "\nReview every file under {} before committing it. Only the login, the password \
         and cookie values are scrubbed: names of kids, teachers and the school may still \
         be in there.",
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

    let current = client
        .timetable_page(None)
        .await
        .context("fetching the current week")?;
    write(&dir.join("timetable-current.html"), &scrub.text(&current))?;

    let options = week_options(&current);
    let next = options
        .iter()
        .position(|(_, selected)| *selected)
        .and_then(|i| options.get(i + 1));
    match next {
        Some((week, _)) => {
            let html = client
                .timetable_page(Some(week))
                .await
                .with_context(|| format!("fetching week {week}"))?;
            write(&dir.join("timetable-next.html"), &scrub.text(&html))?;
        }
        None => eprintln!(
            "warning: {}: no week option after the selected one; skipped the next week",
            dir.display()
        ),
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
        redact_oauth_params(&s)
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
const OAUTH_PARAMS: [&str; 2] = ["code", "state"];

/// Replaces OAuth `code` and `state` values with `<redacted>` anywhere in `text`:
/// URL query parameters (after `?`, `&`, `&amp;` or JSON-escaped `\u0026`) and
/// the `value` of `<input name="code|state">` fields.
fn redact_oauth_params(text: &str) -> String {
    redact_oauth_inputs(&redact_oauth_query(text))
}

fn redact_oauth_query(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let param = OAUTH_PARAMS.iter().find(|p| {
            rest.strip_prefix(**p).is_some_and(|r| r.starts_with('='))
                && (i == 0 || {
                    let before = &text[..i];
                    before.ends_with(['?', '&', ';']) || before.ends_with("\\u0026")
                })
        });
        match param {
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
    let named = OAUTH_PARAMS.iter().any(|p| {
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
    use crate::config::{FamilyConfig, KidConfig, Schedule, WeatherConfig};
    use crate::sources::librus::auth::tests::{Hosts, LOGIN, PASSWORD};
    use chrono::NaiveTime;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, Request, ResponseTemplate};

    const COOKIE_VALUE: &str = "s3ss10n-c00kie-v4lue";

    fn config() -> Config {
        Config {
            timezone: chrono_tz::Europe::Warsaw,
            base_url: "http://x".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            data_dir: "/tmp".into(),
            schedule: Schedule {
                wake: NaiveTime::from_hms_opt(6, 0, 0).unwrap(),
                sleep: NaiveTime::from_hms_opt(22, 0, 0).unwrap(),
                refresh_minutes: 15,
            },
            devices: vec![],
            family: FamilyConfig {
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
}
