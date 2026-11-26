//! Random spreads for poll times.

use std::time::Duration;

use rand::RngExt;
use rand::rngs::StdRng;

/// `base` scaled by a uniform factor in `[1 - spread, 1 + spread]`.
pub fn jittered(base: Duration, spread: f64, rng: &mut StdRng) -> Duration {
    let factor = rng.random_range((1.0 - spread)..=(1.0 + spread));
    base.mul_f64(factor.max(0.0))
}

/// A uniform duration in `[lo, hi)`; `lo` when the range is empty.
pub fn between(lo: Duration, hi: Duration, rng: &mut StdRng) -> Duration {
    if lo >= hi {
        return lo;
    }
    let nanos = rng.random_range(lo.as_nanos()..hi.as_nanos());
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use std::time::Duration;

    const MIN: Duration = Duration::from_secs(60);

    #[test]
    fn jittered_stays_in_bounds() {
        let mut rng = StdRng::seed_from_u64(1);
        let draws: Vec<_> = (0..1000)
            .map(|_| jittered(60 * MIN, 0.25, &mut rng))
            .collect();
        assert!(draws.iter().all(|d| *d >= 45 * MIN && *d <= 75 * MIN));
        assert!(draws.iter().any(|d| *d != draws[0]));
    }

    #[test]
    fn between_degenerate_returns_lo() {
        let mut rng = StdRng::seed_from_u64(1);
        assert_eq!(between(5 * MIN, 5 * MIN, &mut rng), 5 * MIN);
        assert_eq!(between(5 * MIN, 2 * MIN, &mut rng), 5 * MIN);
    }

    #[test]
    fn between_stays_in_range() {
        let mut rng = StdRng::seed_from_u64(2);
        for _ in 0..1000 {
            let d = between(MIN, 8 * MIN, &mut rng);
            assert!(d >= MIN && d < 8 * MIN);
        }
    }
}
