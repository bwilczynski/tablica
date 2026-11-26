//! The family dashboard's plug-in entry.

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, NaiveTime, Utc};
use chrono_tz::Tz;
use serde_json::json;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::dashboards::family::{self, FamilyInputs};
use crate::family::config::{FamilyConfig, KidConfig, parse_config};
use crate::family::polling::{
    CALENDAR_EVERY, INTERVAL_SPREAD, PlannedRuns, TimetablePolicy, WEATHER_EVERY, fresh_rng,
    to_outcome,
};
use crate::family::refresh::{refresh_calendar_once, refresh_timetable_once, refresh_weather_once};
use crate::family::{CALENDAR_MAX_AGE, FamilyCaches, TIMETABLE_MAX_AGE, WEATHER_MAX_AGE};
use crate::sources::librus::LibrusUrls;
use tablica_core::cache::{SourceCache, source_health};
use tablica_core::config::{Loaded, Secrets};
use tablica_core::dashboard::{
    Dashboard, DashboardEntry, ParseResult, ParsedDashboard, PrepareContext, View,
};
use tablica_core::polling::{IntervalPolicy, run_poller};

const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

const OPEN_METEO: &str = "https://api.open-meteo.com";

/// The family section as parsed, plus where its sources live.
pub struct ParsedFamily {
    pub cfg: FamilyConfig,
    pub endpoints: Endpoints,
}

impl ParsedDashboard for ParsedFamily {
    /// Opens the caches and builds the http client. No network traffic.
    fn prepare(self: Box<Self>, ctx: &PrepareContext) -> anyhow::Result<Arc<dyn Dashboard>> {
        let http = reqwest::Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_TIMEOUT)
            .build()
            .context("building the http client")?;
        let caches = Arc::new(FamilyCaches::open(ctx.data_dir, self.cfg.kids.len()));
        Ok(Arc::new(FamilyDashboard {
            cfg: self.cfg,
            tz: ctx.timezone,
            caches,
            http,
            endpoints: self.endpoints,
        }))
    }
}

/// The family dashboard: calendar, weather and per-kid timetables.
pub struct FamilyDashboard {
    cfg: FamilyConfig,
    tz: Tz,
    caches: Arc<FamilyCaches>,
    http: reqwest::Client,
    endpoints: Endpoints,
}

impl Dashboard for FamilyDashboard {
    fn load_templates(&self, env: &mut minijinja::Environment<'static>) {
        minijinja_embed::load_templates!(*env);
    }

    /// Starts the calendar poller, the weather poller and one Librus poller per
    /// kid. They run until `shutdown` is cancelled.
    fn spawn_pollers(self: Arc<Self>, shutdown: CancellationToken) -> Vec<JoinHandle<()>> {
        let tz = self.tz;
        let mut handles = Vec::new();

        {
            let this = self.clone();
            let policy = IntervalPolicy::new(CALENDAR_EVERY, INTERVAL_SPREAD, fresh_rng());
            handles.push(tokio::spawn(run_poller(
                "calendar".into(),
                tz,
                policy,
                shutdown.clone(),
                move || {
                    let this = this.clone();
                    async move {
                        let r = refresh_calendar_once(
                            &this.http,
                            &this.cfg.ics_url,
                            &this.caches.calendar,
                            Utc::now(),
                        )
                        .await;
                        to_outcome(&r)
                    }
                },
            )));
        }

        {
            let this = self.clone();
            let policy = IntervalPolicy::new(WEATHER_EVERY, INTERVAL_SPREAD, fresh_rng());
            handles.push(tokio::spawn(run_poller(
                "weather".into(),
                tz,
                policy,
                shutdown.clone(),
                move || {
                    let this = this.clone();
                    async move {
                        let r = refresh_weather_once(
                            &this.http,
                            &this.endpoints.weather_base,
                            &this.cfg.weather,
                            tz,
                            &this.caches.weather,
                            Utc::now(),
                        )
                        .await;
                        to_outcome(&r)
                    }
                },
            )));
        }

        let planned = PlannedRuns::new(self.cfg.kids.len());
        // Kids with nothing cached fetch at startup; reserve those times before any
        // poller starts so kids with cached data keep their gap from them.
        let empty: Vec<bool> = self
            .caches
            .timetables
            .iter()
            .map(|c| c.get().is_none())
            .collect();
        planned.reserve_startup(Utc::now().with_timezone(&tz), &empty, &mut fresh_rng());
        for (index, kid) in self.cfg.kids.iter().enumerate() {
            let policy =
                TimetablePolicy::new(index, self.caches.clone(), planned.clone(), fresh_rng());
            let this = self.clone();
            handles.push(tokio::spawn(run_poller(
                format!("timetable ({})", kid.label),
                tz,
                policy,
                shutdown.clone(),
                move || {
                    let this = this.clone();
                    async move {
                        let r = refresh_timetable_once(
                            &this.endpoints.librus,
                            &this.cfg.kids[index],
                            &this.caches.timetables[index],
                            Utc::now(),
                        )
                        .await;
                        to_outcome(&r)
                    }
                },
            )));
        }
        handles
    }

    fn view(&self, now: DateTime<Tz>) -> anyhow::Result<View> {
        let calendar = self.caches.calendar.get();
        let weather = self.caches.weather.get();
        let timetables: Vec<_> = self
            .caches
            .timetables
            .iter()
            .map(SourceCache::get)
            .collect();
        let view = family::build(&FamilyInputs {
            now,
            cfg: &self.cfg,
            calendar: calendar.as_ref(),
            weather: weather.as_ref(),
            timetables: timetables.iter().map(Option::as_ref).collect(),
        });
        Ok(View {
            template: "family.html",
            context: minijinja::Value::from_serialize(&view),
        })
    }

    fn switch_at(&self) -> Option<NaiveTime> {
        Some(self.cfg.switch_at)
    }

    fn health(&self, now: DateTime<Utc>) -> serde_json::Value {
        let caches = &self.caches;
        json!({
            "calendar": source_health(&caches.calendar, now, CALENDAR_MAX_AGE),
            "weather": source_health(&caches.weather, now, WEATHER_MAX_AGE),
            "timetables": caches
                .timetables
                .iter()
                .map(|c| source_health(c, now, TIMETABLE_MAX_AGE))
                .collect::<Vec<_>>(),
        })
    }
}

/// External endpoints; tests point them at mock servers.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub weather_base: String,
    pub librus: LibrusUrls,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            weather_base: OPEN_METEO.to_owned(),
            librus: LibrusUrls::default(),
        }
    }
}

pub fn entry() -> DashboardEntry {
    DashboardEntry {
        name: "family",
        parse,
    }
}

/// The kids from the family section as `load` parsed it, for `capture librus`:
/// no second parse, no secret read twice, no caches opened.
pub fn family_kids(loaded: Loaded) -> anyhow::Result<Vec<KidConfig>> {
    let family = loaded
        .dashboards
        .into_iter()
        .find(|d| d.name == "family")
        .context("family dashboard is not active")?;
    let parsed: Box<dyn Any> = family.parsed;
    let parsed = parsed
        .downcast::<ParsedFamily>()
        .map_err(|_| anyhow::anyhow!("family entry did not parse to ParsedFamily"))?;
    Ok(parsed.cfg.kids)
}

fn parse(section: Option<&toml::Value>, secrets: &Secrets) -> ParseResult {
    Ok(Some(Box::new(ParsedFamily {
        cfg: parse_config(section, secrets)?,
        endpoints: Endpoints::default(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboards::test_support::{config, sample_inputs, warsaw};
    use wiremock::MockServer;

    /// A family dashboard over `dir`, seeded with the sample data when `seeded`.
    fn dashboard_in(dir: &tempfile::TempDir, seeded: bool) -> FamilyDashboard {
        let sample = sample_inputs(warsaw(2026, 11, 25, 7, 0));
        if seeded {
            sample.seed(dir.path());
        }
        FamilyDashboard {
            caches: Arc::new(FamilyCaches::open(dir.path(), sample.cfg.kids.len())),
            cfg: sample.cfg,
            tz: chrono_tz::Europe::Warsaw,
            http: reqwest::Client::new(),
            endpoints: Endpoints::default(),
        }
    }

    #[test]
    fn family_view_matches_direct_build() {
        let dir = tempfile::tempdir().unwrap();
        let family = dashboard_in(&dir, true);
        let now = warsaw(2026, 11, 25, 7, 0);
        let sample = sample_inputs(now);
        let view = family.view(now).unwrap();
        assert_eq!(view.template, "family.html");
        assert_eq!(
            view.context,
            minijinja::Value::from_serialize(family::build(&sample.inputs(now)))
        );
        assert_eq!(family.switch_at(), Some(sample.cfg.switch_at));
    }

    #[test]
    fn family_health_shape() {
        let dir = tempfile::tempdir().unwrap();
        let family = dashboard_in(&dir, true);
        let health = family.health(warsaw(2026, 11, 25, 7, 0).with_timezone(&Utc));
        let timetables = health["timetables"].as_array().unwrap();
        assert_eq!(timetables.len(), family.cfg.kids.len());
        let entries = [&health["calendar"], &health["weather"]]
            .into_iter()
            .chain(timetables);
        for entry in entries {
            for key in ["fetched_at", "stale", "last_error"] {
                assert!(entry.get(key).is_some(), "{key} missing in {entry}");
            }
        }
    }

    #[test]
    fn family_health_reports_staleness_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let family = dashboard_in(&dir, true);
        let now = warsaw(2026, 11, 25, 7, 0).with_timezone(&Utc);
        let weather = family.caches.weather.get().unwrap().data;
        family
            .caches
            .weather
            .put(weather, now - chrono::Duration::hours(4))
            .unwrap();
        family
            .caches
            .calendar
            .record_error("http error: timeout".into(), now);

        let health = family.health(now);
        assert_eq!(health["calendar"]["stale"], false);
        assert_eq!(health["calendar"]["last_error"], "http error: timeout");
        assert!(health["calendar"]["fetched_at"].is_string());
        assert_eq!(health["weather"]["stale"], true);
        assert!(health["weather"]["last_error"].is_null());
        assert_eq!(health["timetables"][0]["stale"], false);
    }

    #[test]
    fn healthz_on_empty_caches_reports_stale() {
        let dir = tempfile::tempdir().unwrap();
        let family = dashboard_in(&dir, false);
        let health = family.health(warsaw(2026, 11, 25, 7, 0).with_timezone(&Utc));
        let mut sources = vec![health["calendar"].clone(), health["weather"].clone()];
        sources.extend(health["timetables"].as_array().unwrap().iter().cloned());
        assert_eq!(sources.len(), 2 + family.cfg.kids.len());
        for s in sources {
            assert_eq!(s["stale"], true);
            assert!(s["fetched_at"].is_null());
            assert!(s["last_error"].is_null());
        }
    }

    #[test]
    fn prepare_reads_the_cache_files() {
        let dir = tempfile::tempdir().unwrap();
        let now = warsaw(2026, 11, 25, 7, 0);
        let sample = sample_inputs(now);
        sample.seed(dir.path());
        let ctx = PrepareContext {
            timezone: chrono_tz::Europe::Warsaw,
            data_dir: dir.path(),
        };
        let parsed = ParsedFamily {
            cfg: sample.cfg.clone(),
            endpoints: Endpoints::default(),
        };
        let dashboard = Box::new(parsed).prepare(&ctx).unwrap();
        assert_eq!(
            dashboard.view(now).unwrap().context,
            minijinja::Value::from_serialize(family::build(&sample.inputs(now)))
        );
    }

    #[tokio::test]
    async fn prepare_does_not_touch_the_network() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = PrepareContext {
            timezone: chrono_tz::Europe::Warsaw,
            data_dir: dir.path(),
        };
        let mut cfg = config(2);
        cfg.ics_url = server.uri();
        let parsed = ParsedFamily {
            cfg,
            endpoints: Endpoints {
                weather_base: server.uri(),
                librus: LibrusUrls::default(),
            },
        };
        let dashboard = Box::new(parsed).prepare(&ctx).unwrap();
        drop(dashboard);
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}
