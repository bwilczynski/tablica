//! The family dashboard's plug-in entry.

use crate::config::Secrets;
use crate::dashboard::{DashboardEntry, ParseResult, ParsedDashboard};
use crate::family::config::{FamilyConfig, parse_config};
use crate::sources::librus::LibrusUrls;

const OPEN_METEO: &str = "https://api.open-meteo.com";

/// The family section as parsed, plus where its sources live.
pub struct ParsedFamily {
    pub cfg: FamilyConfig,
    pub endpoints: Endpoints,
}

impl ParsedDashboard for ParsedFamily {}

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

fn parse(section: Option<&toml::Value>, secrets: &Secrets) -> ParseResult {
    Ok(Some(Box::new(ParsedFamily {
        cfg: parse_config(section, secrets)?,
        endpoints: Endpoints::default(),
    })))
}
