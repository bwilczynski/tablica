//! Librus Synergia: login flow, timetable pages and the fixture `capture` command.
//!
//! The login is a Rust port of the `authorize` flow in `Mati365/librus-api` (MIT).

pub mod auth;
pub mod capture;
pub mod timetable;

pub use auth::{Exchange, LibrusClient, LibrusUrls, x_baner};
pub use capture::capture;
