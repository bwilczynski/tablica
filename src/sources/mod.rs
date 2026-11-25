//! Data sources feeding the dashboards.

pub mod calendar;
pub mod librus;
pub mod weather;

/// Failure of a single source fetch.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unexpected http status {0}")]
    Status(u16),
    #[error("cannot parse response: {0}")]
    Parse(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    /// The login POST was refused; retrying would only risk locking the account.
    #[error("credentials rejected: {0}")]
    CredentialsRejected(String),
    #[error("page is not a timetable")]
    NotTimetable,
}
