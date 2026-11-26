//! Exponential retry delays.

use std::time::Duration;

const BACKOFF_BASE_MINUTES: u64 = 5;
const BACKOFF_CAP_MINUTES: u64 = 60;

/// 5 min x 2^attempt, capped at 60 min.
pub fn backoff_delay(attempt: u32) -> Duration {
    let factor = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    let minutes = BACKOFF_BASE_MINUTES
        .saturating_mul(factor)
        .min(BACKOFF_CAP_MINUTES);
    Duration::from_secs(minutes * 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_caps_at_60_min() {
        let mins = |n| backoff_delay(n).as_secs() / 60;
        assert_eq!(mins(0), 5);
        assert_eq!(mins(1), 10);
        assert_eq!(mins(2), 20);
        assert_eq!(mins(3), 40);
        assert_eq!(mins(4), 60);
        assert_eq!(mins(5), 60);
        assert_eq!(mins(500), 60);
    }
}
