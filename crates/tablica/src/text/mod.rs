//! On-screen text. Every string drawn on a dashboard lives under this module.

use serde::{Deserialize, Serialize};

pub mod pl;

/// Weather condition, shared by sources and views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Condition {
    Clear,
    PartlyCloudy,
    Cloudy,
    Fog,
    Rain,
    Snow,
    Storm,
}

/// Kind of expected precipitation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrecipKind {
    Rain,
    Snow,
}
