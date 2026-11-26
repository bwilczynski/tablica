//! The family dashboard: calendar, weather and Librus timetables.

mod caches;
mod config;
mod dashboard;
mod polling;
mod refresh;
mod sources;
mod text;
mod view;

#[cfg(test)]
mod render_tests;
/// Sample data and a seeded dashboard for tests, here and in `tablica`.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use config::{FamilyConfig, KidConfig};
pub use dashboard::{ParsedFamily, entry, family_kids};
pub use sources::librus::capture;
