//! Librus Synergia login chain and authenticated page fetches.
//!
//! Port of `authorize` from `Mati365/librus-api` (MIT), with the end condition from
//! the spec: a redirect to `{synergia}/rodzic/…` or `{synergia}/uczen/…` also means
//! success. Redirects are followed by hand so every hop can be checked.
//!
//! The password only ever goes into the POST body. It never appears in errors, logs
//! or the recorded [`Exchange`]s, which hold responses only.

use std::hash::BuildHasher;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{ACCEPT, LOCATION, ORIGIN, REFERER};
use reqwest::{RequestBuilder, Url, redirect};

use crate::sources::SourceError;

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound on GETs while following the post-login continuation.
const MAX_HOPS: usize = 10;

const STEP1: &str = "step 1 (portalRodzina)";
const STEP2_AUTH: &str = "step 2 (authorization)";
const STEP2_FORM: &str = "step 2 (login form)";
const STEP3: &str = "step 3 (credentials)";
const STEP4: &str = "step 4 (continuation)";

/// Base URLs of the three Librus hosts. Every host check compares against these,
/// so tests can point them at mock servers.
#[derive(Debug, Clone)]
pub struct LibrusUrls {
    pub synergia: Url,
    pub api: Url,
    pub portal: Url,
}

impl Default for LibrusUrls {
    fn default() -> Self {
        Self {
            synergia: Url::parse("https://synergia.librus.pl").expect("valid url"),
            api: Url::parse("https://api.librus.pl").expect("valid url"),
            portal: Url::parse("https://portal.librus.pl").expect("valid url"),
        }
    }
}

/// One HTTP response seen during login, recorded for the `capture` command.
/// Holds the response only; request bodies (with the password) are never recorded.
#[derive(Debug, Clone)]
pub struct Exchange {
    pub method: String,
    pub url: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// A Librus session for one account: its own cookie jar, redirects disabled.
pub struct LibrusClient {
    http: reqwest::Client,
    urls: LibrusUrls,
}

/// The `x-baner` header, as upstream builds it: `String(Math.random())` and
/// `Date.now()`, each character shifted by +20 code points, joined with `_`.
/// `random` is in `[0, 1)`; Rust's shortest round-trip `Display` matches JS for it.
pub fn x_baner(random: f64, now_ms: u64) -> String {
    fn shift(s: &str) -> String {
        s.chars()
            .map(|c| char::from_u32(c as u32 + 20).unwrap_or(c))
            .collect()
    }
    format!(
        "{}_{}",
        shift(&random.to_string()),
        shift(&now_ms.to_string())
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A random value in `[0, 1)` for `x-baner`, without pulling in an RNG crate.
fn random_unit() -> f64 {
    let bits = std::collections::hash_map::RandomState::new().hash_one(now_ms());
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

/// `host[:port]/path` of a URL, for errors and logs. Never includes the query,
/// which carries OAuth state.
fn describe(url: &Url) -> String {
    let host = url.host_str().unwrap_or("?");
    match url.port() {
        Some(port) => format!("{host}:{port}{}", url.path()),
        None => format!("{host}{}", url.path()),
    }
}

fn auth(step: &str, msg: String) -> SourceError {
    SourceError::Auth(format!("{step}: {msg}"))
}

/// `{base}{path}`, keeping any path prefix the base has.
fn endpoint(base: &Url, path: &str) -> Url {
    Url::parse(&format!("{}{path}", base.as_str().trim_end_matches('/')))
        .expect("base url joined with a constant path")
}

/// `url` is on the same origin as `base`, without credentials, and its path is
/// `{base}{path}` exactly or below it (`{base}{path}/…`).
fn is_at(url: &Url, base: &Url, path: &str) -> bool {
    let full = format!("{}{path}", base.path().trim_end_matches('/'));
    url.origin() == base.origin()
        && url.username().is_empty()
        && url.password().is_none()
        && (url.path() == full
            || url
                .path()
                .strip_prefix(&full)
                .is_some_and(|rest| rest.starts_with('/')))
}

/// The `goTo` field of a JSON body, if any.
fn json_goto(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("goTo")?.as_str().map(str::to_owned)
}

struct Page {
    status: u16,
    location: Option<String>,
    body: String,
}

impl Page {
    fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    fn is_redirect(&self) -> bool {
        (300..400).contains(&self.status)
    }
}

type Trace<'a> = Option<&'a mut Vec<Exchange>>;

impl LibrusClient {
    pub fn new(urls: LibrusUrls) -> Self {
        let http = reqwest::Client::builder()
            .cookie_store(true)
            .redirect(redirect::Policy::none())
            .user_agent(USER_AGENT)
            .timeout(TIMEOUT)
            .build()
            .expect("building the Librus http client");
        Self { http, urls }
    }

    fn is_api_authorization(&self, url: &Url) -> bool {
        is_at(url, &self.urls.api, "/OAuth/Authorization")
    }

    /// An allowed hop of the post-login chain.
    fn is_chain(&self, url: &Url) -> bool {
        self.is_api_authorization(url) || is_at(url, &self.urls.synergia, "/loguj/portalRodzina")
    }

    /// A logged-in landing page: reaching it ends the login.
    fn is_landing(&self, url: &Url) -> bool {
        is_at(url, &self.urls.synergia, "/rodzic") || is_at(url, &self.urls.synergia, "/uczen")
    }

    async fn send(
        &self,
        step: &str,
        method: &str,
        url: &Url,
        req: RequestBuilder,
        trace: &mut Trace<'_>,
    ) -> Result<Page, SourceError> {
        let fail = |e: reqwest::Error| {
            auth(
                step,
                format!("request to {} failed: {}", describe(url), e.without_url()),
            )
        };
        let resp = req.send().await.map_err(fail)?;
        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_owned(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let location = resp
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = resp.text().await.map_err(fail)?;
        tracing::debug!(step, method, url = %describe(url), status, "librus login");
        if let Some(t) = trace.as_deref_mut() {
            t.push(Exchange {
                method: method.to_owned(),
                url: url.to_string(),
                status,
                headers,
                body: body.clone(),
            });
        }
        Ok(Page {
            status,
            location,
            body,
        })
    }

    async fn get(
        &self,
        step: &str,
        url: &Url,
        referer: &Url,
        trace: &mut Trace<'_>,
    ) -> Result<Page, SourceError> {
        let req = self.http.get(url.clone()).header(REFERER, referer.as_str());
        self.send(step, "GET", url, req, trace).await
    }

    /// The redirect target of `page`, which must be on `{api}/OAuth/Authorization…`.
    fn authorization_redirect(
        &self,
        step: &str,
        url: &Url,
        page: &Page,
    ) -> Result<Url, SourceError> {
        let location = match (&page.location, page.is_redirect()) {
            (Some(loc), true) => loc,
            _ => {
                return Err(auth(
                    step,
                    format!(
                        "expected a redirect, got status {} at {}",
                        page.status,
                        describe(url)
                    ),
                ));
            }
        };
        let next = resolve(step, url, location)?;
        if !self.is_api_authorization(&next) {
            return Err(auth(
                step,
                format!("unexpected redirect: {}", describe(&next)),
            ));
        }
        Ok(next)
    }

    /// Logs in, leaving the session cookies in this client's jar. With `trace`,
    /// every response of the chain is appended to it.
    pub async fn login(
        &self,
        login: &str,
        password: &str,
        mut trace: Option<&mut Vec<Exchange>>,
    ) -> Result<(), SourceError> {
        let trace = &mut trace;

        // 1. portalRodzina redirects to the OAuth authorization, carrying its state.
        let portal_rodzina = endpoint(&self.urls.synergia, "/loguj/portalRodzina");
        let portal = endpoint(&self.urls.portal, "/");
        let page = self.get(STEP1, &portal_rodzina, &portal, trace).await?;
        let authorization = self.authorization_redirect(STEP1, &portal_rodzina, &page)?;

        // 2. The authorization sets the API session and redirects to the login form.
        let page = self
            .get(STEP2_AUTH, &authorization, &portal_rodzina, trace)
            .await?;
        let login_url = self.authorization_redirect(STEP2_AUTH, &authorization, &page)?;
        let page = self
            .get(STEP2_FORM, &login_url, &authorization, trace)
            .await?;
        // Any status below 400 is fine here, as upstream accepts.
        if !(200..400).contains(&page.status) {
            return Err(auth(
                STEP2_FORM,
                format!("status {} at {}", page.status, describe(&login_url)),
            ));
        }

        // 3. Submit the credentials with the headers the browser sends.
        let origin = self.urls.api.origin().ascii_serialization();
        let req = self
            .http
            .post(login_url.clone())
            .header("x-baner", x_baner(random_unit(), now_ms()))
            .header("X-Requested-With", "XMLHttpRequest")
            .header(ACCEPT, "application/json, text/javascript, */*; q=0.01")
            .header(REFERER, login_url.as_str())
            .header(ORIGIN, origin)
            .form(&[("action", "login"), ("login", login), ("pass", password)]);
        let page = self.send(STEP3, "POST", &login_url, req, trace).await?;
        if page.status >= 400 {
            return Err(auth(
                STEP3,
                format!(
                    "status {} at {} (credentials rejected?)",
                    page.status,
                    describe(&login_url)
                ),
            ));
        }
        let next = page
            .location
            .clone()
            .or_else(|| json_goto(&page.body))
            .ok_or_else(|| {
                auth(
                    STEP3,
                    format!(
                        "no continuation (Location or goTo) from {}",
                        describe(&login_url)
                    ),
                )
            })?;
        let next = resolve(STEP3, &login_url, &next)?;

        // 4–6. Follow the continuation by hand, checking every hop.
        self.follow_chain(next, login_url, trace).await
    }

    async fn follow_chain(
        &self,
        mut next: Url,
        mut referer: Url,
        trace: &mut Trace<'_>,
    ) -> Result<(), SourceError> {
        for _ in 0..MAX_HOPS {
            if self.is_landing(&next) {
                return Ok(());
            }
            if !self.is_chain(&next) {
                return Err(auth(
                    STEP4,
                    format!("unexpected redirect: {}", describe(&next)),
                ));
            }
            let page = self.get(STEP4, &next, &referer, trace).await?;
            if page.is_success() {
                return Ok(());
            }
            match (&page.location, page.is_redirect()) {
                (Some(loc), true) => {
                    let hop = resolve(STEP4, &next, loc)?;
                    referer = std::mem::replace(&mut next, hop);
                }
                _ => {
                    return Err(auth(
                        STEP4,
                        format!("status {} at {}", page.status, describe(&next)),
                    ));
                }
            }
        }
        if self.is_landing(&next) {
            return Ok(());
        }
        Err(auth(
            STEP4,
            format!(
                "more than {MAX_HOPS} redirects, last to {}",
                describe(&next)
            ),
        ))
    }

    /// A timetable page. `None` is the current week (`GET`); `Some(v)` posts the
    /// week selector `tydzien=v` as multipart, as the page's own form does.
    pub async fn timetable_page(&self, week: Option<&str>) -> Result<String, SourceError> {
        let url = endpoint(&self.urls.synergia, "/przegladaj_plan_lekcji");
        let req = match week {
            None => self.http.get(url),
            Some(v) => self
                .http
                .post(url)
                .multipart(reqwest::multipart::Form::new().text("tydzien", v.to_owned())),
        };
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(SourceError::Status(status.as_u16()));
        }
        Ok(resp.text().await?)
    }
}

/// Resolves a `Location`/`goTo` value against the URL that returned it.
fn resolve(step: &str, base: &Url, location: &str) -> Result<Url, SourceError> {
    base.join(location).map_err(|_| {
        auth(
            step,
            format!("invalid redirect location from {}", describe(base)),
        )
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    pub(crate) const LOGIN: &str = "jan.kowalski7";
    pub(crate) const PASSWORD: &str = "Sup3r-Secret!";

    /// Three mock servers standing in for synergia, api and portal.
    pub(crate) struct Hosts {
        pub synergia: MockServer,
        pub api: MockServer,
        pub portal: MockServer,
    }

    impl Hosts {
        pub(crate) async fn start() -> Self {
            Self {
                synergia: MockServer::start().await,
                api: MockServer::start().await,
                portal: MockServer::start().await,
            }
        }

        pub(crate) fn urls(&self) -> LibrusUrls {
            LibrusUrls {
                synergia: Url::parse(&self.synergia.uri()).unwrap(),
                api: Url::parse(&self.api.uri()).unwrap(),
                portal: Url::parse(&self.portal.uri()).unwrap(),
            }
        }

        /// Steps 1 and 2: portalRodzina → OAuth authorization → login form.
        pub(crate) async fn mount_entry(&self) {
            Mock::given(method("GET"))
                .and(path("/loguj/portalRodzina"))
                .respond_with(ResponseTemplate::new(302).insert_header(
                    "Location",
                    format!(
                        "{}/OAuth/Authorization?client_id=46&response_type=code&state=s1",
                        self.api.uri()
                    ),
                ))
                .with_priority(10)
                .mount(&self.synergia)
                .await;
            Mock::given(method("GET"))
                .and(path("/OAuth/Authorization"))
                .and(query_param("response_type", "code"))
                .respond_with(
                    ResponseTemplate::new(302)
                        .insert_header("Location", "/OAuth/Authorization?client_id=46")
                        .insert_header("Set-Cookie", "oauth_token=tok-c00kie-1; path=/"),
                )
                .with_priority(1)
                .mount(&self.api)
                .await;
            Mock::given(method("GET"))
                .and(path("/OAuth/Authorization"))
                .respond_with(ResponseTemplate::new(200).set_body_string("<form>login form</form>"))
                .with_priority(5)
                .mount(&self.api)
                .await;
        }

        /// The POST answers with a redirect to `location`.
        pub(crate) async fn mount_post_redirect(&self, location: &str) {
            Mock::given(method("POST"))
                .and(path("/OAuth/Authorization"))
                .respond_with(ResponseTemplate::new(302).insert_header("Location", location))
                .mount(&self.api)
                .await;
        }

        /// Standard tail: 2FA hop → synergia portalRodzina?code → `final_response`.
        pub(crate) async fn mount_tail(&self, final_response: ResponseTemplate) {
            Mock::given(method("GET"))
                .and(path("/OAuth/Authorization/2FA"))
                .respond_with(ResponseTemplate::new(302).insert_header(
                    "Location",
                    format!("{}/loguj/portalRodzina?code=abc", self.synergia.uri()),
                ))
                .mount(&self.api)
                .await;
            Mock::given(method("GET"))
                .and(path("/loguj/portalRodzina"))
                .and(query_param("code", "abc"))
                .respond_with(final_response)
                .with_priority(1)
                .mount(&self.synergia)
                .await;
        }
    }

    fn client(h: &Hosts) -> LibrusClient {
        LibrusClient::new(h.urls())
    }

    #[test]
    fn x_baner_matches_upstream_format() {
        assert_eq!(x_baner(0.5, 3), "DBI_G");
        assert_eq!(x_baner(0.25, 1_700_000_000_000), "DBFI_EKDDDDDDDDDDD");
    }

    #[tokio::test]
    async fn login_follows_chain_and_ends_on_rodzic_redirect() {
        let h = Hosts::start().await;
        h.mount_entry().await;
        h.mount_post_redirect("/OAuth/Authorization/2FA?client_id=46")
            .await;
        h.mount_tail(ResponseTemplate::new(302).insert_header("Location", "/rodzic/index"))
            .await;
        Mock::given(path("/rodzic/index"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&h.synergia)
            .await;

        let mut trace = Vec::new();
        client(&h)
            .login(LOGIN, PASSWORD, Some(&mut trace))
            .await
            .unwrap();
        // portalRodzina, authorization, form, POST, 2FA, portalRodzina?code
        assert_eq!(trace.len(), 6, "{trace:#?}");
        assert_eq!(trace[3].method, "POST");
        assert_eq!(trace[5].status, 302);
    }

    #[tokio::test]
    async fn login_form_may_answer_with_a_redirect() {
        let h = Hosts::start().await;
        h.mount_entry().await;
        // The login form GET answers 302 (upstream accepts any status below 400).
        Mock::given(method("GET"))
            .and(path("/OAuth/Authorization"))
            .and(query_param("client_id", "46"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "/OAuth/Authorization/Form"),
            )
            .with_priority(2)
            .expect(1)
            .mount(&h.api)
            .await;
        h.mount_post_redirect(&format!("{}/rodzic/index", h.synergia.uri()))
            .await;
        client(&h).login(LOGIN, PASSWORD, None).await.unwrap();
    }

    #[tokio::test]
    async fn login_form_error_status_fails_step_2() {
        let h = Hosts::start().await;
        h.mount_entry().await;
        Mock::given(method("GET"))
            .and(path("/OAuth/Authorization"))
            .and(query_param("client_id", "46"))
            .respond_with(ResponseTemplate::new(404))
            .with_priority(2)
            .mount(&h.api)
            .await;
        let err = client(&h).login(LOGIN, PASSWORD, None).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("step 2 (login form)") && msg.contains("404"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn login_ends_on_200_from_portal_rodzina() {
        let h = Hosts::start().await;
        h.mount_entry().await;
        h.mount_post_redirect("/OAuth/Authorization/2FA?client_id=46")
            .await;
        h.mount_tail(ResponseTemplate::new(200).set_body_string("<html>ok</html>"))
            .await;
        client(&h).login(LOGIN, PASSWORD, None).await.unwrap();
    }

    #[tokio::test]
    async fn login_accepts_goto_json_continuation() {
        let h = Hosts::start().await;
        h.mount_entry().await;
        Mock::given(method("POST"))
            .and(path("/OAuth/Authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "goTo": "/OAuth/Authorization/2FA?client_id=46"
            })))
            .mount(&h.api)
            .await;
        Mock::given(method("GET"))
            .and(path("/OAuth/Authorization/2FA"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/uczen/index", h.synergia.uri())),
            )
            .expect(1)
            .mount(&h.api)
            .await;
        client(&h).login(LOGIN, PASSWORD, None).await.unwrap();
    }

    #[tokio::test]
    async fn login_rejects_foreign_host() {
        let h = Hosts::start().await;
        h.mount_entry().await;
        h.mount_post_redirect("http://evil.example/steal?token=x")
            .await;
        let err = client(&h).login(LOGIN, PASSWORD, None).await.unwrap_err();
        assert!(matches!(err, SourceError::Auth(_)), "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains("unexpected redirect: evil.example/steal"),
            "{msg}"
        );
        assert!(!msg.contains("token"), "{msg}");

        // The right host but a path outside the allowed chain is rejected too.
        let h = Hosts::start().await;
        h.mount_entry().await;
        h.mount_post_redirect("/OAuth/Other").await;
        let err = client(&h).login(LOGIN, PASSWORD, None).await.unwrap_err();
        assert!(
            err.to_string().contains("unexpected redirect:")
                && err.to_string().contains("/OAuth/Other"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn login_stops_after_10_hops() {
        let h = Hosts::start().await;
        h.mount_entry().await;
        h.mount_post_redirect("/OAuth/Authorization/Loop").await;
        Mock::given(method("GET"))
            .and(path("/OAuth/Authorization/Loop"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "/OAuth/Authorization/Loop"),
            )
            .expect(10)
            .mount(&h.api)
            .await;
        let err = client(&h).login(LOGIN, PASSWORD, None).await.unwrap_err();
        assert!(matches!(err, SourceError::Auth(_)), "{err}");
        assert!(err.to_string().contains("10"), "{err}");
    }

    #[tokio::test]
    async fn login_error_never_contains_password() {
        let h = Hosts::start().await;
        h.mount_entry().await;
        Mock::given(method("POST"))
            .and(path("/OAuth/Authorization"))
            .respond_with(ResponseTemplate::new(403).set_body_string(PASSWORD))
            .mount(&h.api)
            .await;
        let err = client(&h).login(LOGIN, PASSWORD, None).await.unwrap_err();
        assert!(matches!(err, SourceError::Auth(_)), "{err}");
        assert!(!err.to_string().contains(PASSWORD), "{err}");
        assert!(err.to_string().contains("403"), "{err}");
    }

    #[tokio::test]
    async fn post_carries_required_headers_and_form() {
        let h = Hosts::start().await;
        h.mount_entry().await;
        let api = h.api.uri();
        let login_url = format!("{api}/OAuth/Authorization?client_id=46");
        let matcher = move |req: &Request| {
            let hv = |name: &str| {
                req.headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            };
            let baner_ok = hv("x-baner").is_some_and(|v| {
                let (a, b) = v.split_once('_').unwrap_or(("", ""));
                !a.is_empty()
                    && !b.is_empty()
                    && a.starts_with("DB")
                    && v.chars()
                        .all(|c| c == '_' || c == 'B' || ('D'..='M').contains(&c))
            });
            baner_ok
                && hv("x-requested-with").as_deref() == Some("XMLHttpRequest")
                && hv("accept").as_deref() == Some("application/json, text/javascript, */*; q=0.01")
                && hv("referer").as_deref() == Some(login_url.as_str())
                && hv("origin").as_deref() == Some(api.as_str())
                && hv("content-type").as_deref() == Some("application/x-www-form-urlencoded")
                && req.body == b"action=login&login=jan.kowalski7&pass=Sup3r-Secret%21"
        };
        Mock::given(method("POST"))
            .and(path("/OAuth/Authorization"))
            .and(matcher)
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/rodzic/index", h.synergia.uri())),
            )
            .with_priority(1)
            .expect(1)
            .mount(&h.api)
            .await;
        Mock::given(method("POST"))
            .and(path("/OAuth/Authorization"))
            .respond_with(ResponseTemplate::new(400))
            .with_priority(10)
            .mount(&h.api)
            .await;
        client(&h).login(LOGIN, PASSWORD, None).await.unwrap();
    }

    #[tokio::test]
    async fn timetable_page_gets_and_posts_multipart() {
        let h = Hosts::start().await;
        Mock::given(method("GET"))
            .and(path("/przegladaj_plan_lekcji"))
            .respond_with(ResponseTemplate::new(200).set_body_string("current"))
            .mount(&h.synergia)
            .await;
        Mock::given(method("POST"))
            .and(path("/przegladaj_plan_lekcji"))
            .and(|req: &Request| {
                let body = String::from_utf8_lossy(&req.body);
                req.headers
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.starts_with("multipart/form-data"))
                    && body.contains("name=\"tydzien\"")
                    && body.contains("2026-11-30_2026-12-06")
            })
            .respond_with(ResponseTemplate::new(200).set_body_string("next"))
            .mount(&h.synergia)
            .await;
        let c = client(&h);
        assert_eq!(c.timetable_page(None).await.unwrap(), "current");
        assert_eq!(
            c.timetable_page(Some("2026-11-30_2026-12-06"))
                .await
                .unwrap(),
            "next"
        );
    }
}
