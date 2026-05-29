//! EWMA smoothing — pure formula extracted from engine state.rs:245-254.

use rust_decimal::Decimal;

/// Apply one EWMA step. When `prev` is `None`, result seeds at `raw`.
/// Otherwise: `alpha * raw + (1 - alpha) * prev`.
pub fn apply_ewma_pure(raw: Decimal, prev: Option<Decimal>, alpha: Decimal) -> Decimal {
    match prev {
        None => raw,
        Some(p) => alpha * raw + (Decimal::ONE - alpha) * p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::str::FromStr;

    #[test]
    fn seeds_at_raw_when_no_prev() {
        let raw = Decimal::from_str("0.01").unwrap();
        assert_eq!(
            apply_ewma_pure(raw, None, Decimal::from_str("0.3").unwrap()),
            raw
        );
    }

    #[test]
    fn classic_blend() {
        let r = apply_ewma_pure(
            Decimal::ZERO,
            Some(Decimal::from_str("0.01").unwrap()),
            Decimal::from_str("0.3").unwrap(),
        );
        assert_eq!(r, Decimal::from_str("0.007").unwrap());
    }

    #[test]
    fn alpha_zero_returns_prev() {
        let prev = Decimal::from_str("0.005").unwrap();
        let raw = Decimal::from_str("999").unwrap();
        assert_eq!(apply_ewma_pure(raw, Some(prev), Decimal::ZERO), prev);
    }

    #[test]
    fn alpha_one_returns_raw() {
        let prev = Decimal::from_str("0.005").unwrap();
        let raw = Decimal::from_str("0.999").unwrap();
        assert_eq!(apply_ewma_pure(raw, Some(prev), Decimal::ONE), raw);
    }

    proptest! {
        #[test]
        fn output_bounded_between_raw_and_prev(
            raw_int in -1_000_000i64..1_000_000i64,
            prev_int in -1_000_000i64..1_000_000i64,
            alpha_int in 0u32..=100,
        ) {
            let raw = Decimal::new(raw_int, 6);
            let prev = Decimal::new(prev_int, 6);
            let alpha = Decimal::new(alpha_int as i64, 2);
            let out = apply_ewma_pure(raw, Some(prev), alpha);
            let lo = raw.min(prev);
            let hi = raw.max(prev);
            prop_assert!(out >= lo && out <= hi, "out={} not in [{}, {}]", out, lo, hi);
        }
    }
}
