//! EWMA smoothing — pure formula extracted from engine state.rs:245-254.

use rust_decimal::Decimal;

/// Apply one EWMA step. When `prev` is `None`, result seeds at `raw`.
/// Otherwise: `alpha * raw + (1 - alpha) * prev`.
///
/// Returns `None` on arithmetic overflow (audit F-12 class: `rust_decimal`'s
/// `*`/`+` PANIC on overflow rather than erroring). In the live engine this
/// is unreachable in practice — `raw` always passes `is_plausible_discount_sample`
/// (`|raw| < 1`) before reaching here, and by induction `prev` (a prior output
/// of this same function) stays within the same bound, so the convex
/// combination can never overflow. It IS reachable through `rederive`, whose
/// `ewma_prev`/`alpha` come straight from an untrusted receipt with no such
/// invariant — `None` there means "cannot re-derive," never "confirmed
/// healthy" (H8: missing data ≠ confirming data).
pub fn apply_ewma_pure(raw: Decimal, prev: Option<Decimal>, alpha: Decimal) -> Option<Decimal> {
    match prev {
        None => Some(raw),
        Some(p) => {
            let a = alpha.checked_mul(raw)?;
            let one_minus_alpha = Decimal::ONE.checked_sub(alpha)?;
            let b = one_minus_alpha.checked_mul(p)?;
            a.checked_add(b)
        }
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
            Some(raw)
        );
    }

    #[test]
    fn classic_blend() {
        let r = apply_ewma_pure(
            Decimal::ZERO,
            Some(Decimal::from_str("0.01").unwrap()),
            Decimal::from_str("0.3").unwrap(),
        );
        assert_eq!(r, Some(Decimal::from_str("0.007").unwrap()));
    }

    #[test]
    fn alpha_zero_returns_prev() {
        let prev = Decimal::from_str("0.005").unwrap();
        let raw = Decimal::from_str("999").unwrap();
        assert_eq!(apply_ewma_pure(raw, Some(prev), Decimal::ZERO), Some(prev));
    }

    #[test]
    fn alpha_one_returns_raw() {
        let prev = Decimal::from_str("0.005").unwrap();
        let raw = Decimal::from_str("0.999").unwrap();
        assert_eq!(apply_ewma_pure(raw, Some(prev), Decimal::ONE), Some(raw));
    }

    /// Regression: `rederive`'s `ewma_prev`/`alpha` come straight from an
    /// untrusted receipt with no bound on their magnitude (unlike the live
    /// engine, where `raw`/`prev` are always `|x| < 1` by construction — see
    /// the function doc). `raw` and `prev` both near `Decimal::MAX` previously
    /// panicked ("Addition overflowed") on the final sum instead of failing
    /// the re-derivation cleanly.
    #[test]
    fn overflowing_sum_returns_none_not_panic() {
        let r = apply_ewma_pure(
            Decimal::MAX,
            Some(Decimal::MAX),
            Decimal::from_str("0.5").unwrap(),
        );
        assert_eq!(r, None, "overflowing EWMA sum must return None, not panic");
    }

    /// Regression: a very large-magnitude negative `alpha` previously panicked
    /// ("Subtraction overflowed") on the `1 - alpha` step alone, before either
    /// multiplication ran.
    #[test]
    fn overflowing_one_minus_alpha_returns_none_not_panic() {
        let r = apply_ewma_pure(
            Decimal::from_str("0.01").unwrap(),
            Some(Decimal::from_str("0.01").unwrap()),
            Decimal::MIN,
        );
        assert_eq!(
            r, None,
            "overflowing (1 - alpha) must return None, not panic"
        );
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
            let out = apply_ewma_pure(raw, Some(prev), alpha).expect("bounded inputs never overflow");
            let lo = raw.min(prev);
            let hi = raw.max(prev);
            prop_assert!(out >= lo && out <= hi, "out={} not in [{}, {}]", out, lo, hi);
        }
    }
}
