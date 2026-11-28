//! The plug-in contract between core and each dashboard.

use std::any::Any;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Duration, NaiveTime, Utc};
use chrono_tz::Tz;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::cache::{CacheStatus, is_stale};
use crate::config::{ConfigError, Secrets};
use crate::diagnostics::Section;
use crate::metrics::Recorder;

/// What an entry's `parse` returns: `Ok(None)` when the dashboard is inactive.
pub type ParseResult = Result<Option<Box<dyn ParsedDashboard>>, ConfigError>;

/// One entry per dashboard, listed in the binary.
#[derive(Clone, Copy)]
pub struct DashboardEntry {
    /// Canonical key: config section, device `dashboard = "..."`, preview stem,
    /// image filename prefix.
    pub name: &'static str,
    /// Parses and validates the section (`None` when absent from the file),
    /// reading secrets. No caches, no clients, no network. `Ok(None)` means the
    /// dashboard is not active.
    pub parse: fn(Option<&toml::Value>, &Secrets) -> ParseResult,
}

/// A dashboard's parsed config. `Any` so the binary can recover the typed value.
pub trait ParsedDashboard: Any + Send {
    /// Opens caches and builds clients. Must not touch the network.
    fn prepare(self: Box<Self>, ctx: &PrepareContext) -> anyhow::Result<Arc<dyn Dashboard>>;
}

/// The prepared dashboards, keyed by their entry name.
pub type Active = BTreeMap<&'static str, Arc<dyn Dashboard>>;

pub struct PrepareContext<'a> {
    pub timezone: Tz,
    pub data_dir: &'a Path,
}

pub trait Dashboard: Send + Sync {
    /// Registers the embedded templates into this dashboard's own environment.
    fn load_templates(&self, env: &mut minijinja::Environment<'static>);
    /// Starts background pollers. Infallible.
    fn spawn_pollers(
        self: Arc<Self>,
        shutdown: CancellationToken,
        recorder: Recorder,
    ) -> Vec<JoinHandle<()>>;
    /// Template name and context for a render at `now`. Reads caches only.
    fn view(&self, now: DateTime<Tz>) -> anyhow::Result<View>;
    /// A local time of day the device should wake just after.
    fn switch_at(&self) -> Option<NaiveTime> {
        None
    }
    /// JSON for `/healthz`.
    fn health(&self, now: DateTime<Utc>) -> serde_json::Value;
    /// Sections for the device diagnostics screen. Reads caches only.
    fn diagnostics(&self, _now: DateTime<Utc>) -> Vec<Section> {
        Vec::new()
    }
    /// Per-source freshness for metrics. Reads caches only.
    fn sources(&self, _now: DateTime<Utc>) -> Vec<SourceState> {
        Vec::new()
    }
}

/// One source's state at scrape time.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceState {
    pub id: String,
    pub last_success: Option<DateTime<Utc>>,
    pub failing: bool,
    pub stale: bool,
}

impl SourceState {
    /// Stale when nothing was ever fetched or the data is older than `max_age`.
    pub fn from_status(
        id: impl Into<String>,
        s: CacheStatus,
        now: DateTime<Utc>,
        max_age: Duration,
    ) -> SourceState {
        SourceState {
            id: id.into(),
            last_success: s.fetched_at,
            failing: s.failing,
            stale: s.fetched_at.is_none_or(|t| is_stale(t, now, max_age)),
        }
    }
}

pub struct View {
    pub template: &'static str,
    pub context: minijinja::Value,
}

/// Core's own top-level config keys, which no dashboard may claim.
const RESERVED: [&str; 7] = [
    "timezone",
    "base_url",
    "listen",
    "metrics_listen",
    "data_dir",
    "schedule",
    "device",
];

/// Checks every entry name: `[a-z]{5,7}`, not a core key, unique. Five to seven
/// letters keep the firmware's 13-character image identity clear of the timestamp.
pub fn validate_names(entries: &[DashboardEntry]) -> Result<(), ConfigError> {
    let mut seen = HashSet::new();
    for entry in entries {
        let name = entry.name;
        let reason = if !(5..=7).contains(&name.len()) {
            Some("must be 5 to 7 characters long")
        } else if !name.bytes().all(|b| b.is_ascii_lowercase()) {
            Some("must contain only lowercase ASCII letters")
        } else if RESERVED.contains(&name) {
            Some("is a reserved top-level config key")
        } else if !seen.insert(name) {
            Some("is registered twice")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(ConfigError::InvalidDashboardName {
                name: name.to_owned(),
                reason,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigError, Secrets};

    fn stub_parse(_: Option<&toml::Value>, _: &Secrets) -> ParseResult {
        Ok(None)
    }

    fn entries(names: &[&'static str]) -> Vec<DashboardEntry> {
        names
            .iter()
            .map(|&name| DashboardEntry {
                name,
                parse: stub_parse,
            })
            .collect()
    }

    fn rejected(names: &[&'static str]) -> bool {
        matches!(
            validate_names(&entries(names)),
            Err(ConfigError::InvalidDashboardName { .. })
        )
    }

    #[test]
    fn validate_names_rules() {
        assert!(validate_names(&entries(&["family", "homelab", "abcde"])).is_ok());
        for bad in ["abcd", "abcdefgh", "Family", "fam1ly"] {
            assert!(rejected(&[bad]), "{bad} accepted");
        }
        assert!(rejected(&["family", "family"]), "duplicate accepted");
        for reserved in ["device", "listen"] {
            assert!(rejected(&[reserved]), "{reserved} accepted");
        }
        // Longer than 7 characters, so name-length validation never reaches it.
        assert!(RESERVED.contains(&"metrics_listen"));
    }

    #[test]
    fn source_state_stale_rules() {
        use chrono::TimeZone;
        let now = Utc.with_ymd_and_hms(2026, 11, 25, 12, 0, 0).unwrap();
        let max = Duration::hours(2);
        let state = |fetched_at| {
            SourceState::from_status(
                "x",
                CacheStatus {
                    fetched_at,
                    failing: false,
                },
                now,
                max,
            )
        };
        assert!(state(None).stale);
        assert_eq!(state(None).last_success, None);
        assert!(state(Some(now - Duration::hours(3))).stale);
        assert!(!state(Some(now - Duration::hours(1))).stale);
    }
}
