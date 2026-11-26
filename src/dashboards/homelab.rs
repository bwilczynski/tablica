//! Homelab dashboard view model (placeholder until the dashboard exists).

use serde::Serialize;

use crate::config::Secrets;
use crate::dashboard::{DashboardEntry, ParseResult, ParsedDashboard};
use crate::text::pl;

/// Homelab takes no config; any `[homelab]` keys are ignored.
pub struct ParsedHomelab;

impl ParsedDashboard for ParsedHomelab {}

/// Always active.
pub fn entry() -> DashboardEntry {
    DashboardEntry {
        name: "homelab",
        parse,
    }
}

fn parse(_section: Option<&toml::Value>, _secrets: &Secrets) -> ParseResult {
    Ok(Some(Box::new(ParsedHomelab)))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HomelabView {
    pub message: &'static str,
}

pub fn build() -> HomelabView {
    HomelabView {
        message: pl::HOMELAB_PLACEHOLDER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_placeholder() {
        assert_eq!(build().message, "Homelab: wkrótce");
    }
}
