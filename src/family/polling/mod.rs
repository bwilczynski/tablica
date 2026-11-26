//! How the family dashboard's sources are polled.

pub mod timetable_policy;
pub mod windows;

use std::time::Duration;

use rand::rngs::StdRng;

use crate::polling::Outcome;
use crate::sources::SourceError;

pub use timetable_policy::{PlannedRuns, TimetablePolicy};

pub(crate) const CALENDAR_EVERY: Duration = Duration::from_secs(15 * 60);
pub(crate) const WEATHER_EVERY: Duration = Duration::from_secs(30 * 60);
pub(crate) const INTERVAL_SPREAD: f64 = 0.2;

/// How a fetch went, as far as scheduling cares.
pub(crate) fn to_outcome(r: &Result<(), SourceError>) -> Outcome {
    match r {
        Ok(()) => Outcome::Ok,
        Err(SourceError::CredentialsRejected(_)) => Outcome::Rejected,
        Err(_) => Outcome::Failed,
    }
}

pub(crate) fn fresh_rng() -> StdRng {
    rand::make_rng()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_outcome_maps_rejected_credentials() {
        assert_eq!(to_outcome(&Ok(())), Outcome::Ok);
        assert_eq!(
            to_outcome(&Err(SourceError::CredentialsRejected("x".into()))),
            Outcome::Rejected
        );
        assert_eq!(to_outcome(&Err(SourceError::Status(500))), Outcome::Failed);
    }
}
