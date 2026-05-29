//! Discount computation and plausibility checks.

use pegana_common_verify::AssetClass;
use rust_decimal::Decimal;

/// Compute signed discount `1 - market/intrinsic`.
///
/// For LST class we prefer the SOL-denominated path when both sides have
/// published a SOL value: `intrinsic_usd` and `market_usd` are both
/// `pyth(SOL/USD) × …` and Sanctum vs Jupiter may have cached different
/// SOL/USD snapshots — computing in SOL cancels the multiplier. Stables /
/// yield / FX assets stay on the USD path because the USD value IS the
/// invariant for those.
///
/// Returns `Decimal::ZERO` when `intrinsic` is zero (avoids div-by-zero;
/// engine treats that as "no signal, skip publish").
pub fn compute_discount(
    intrinsic: Decimal,
    market: Decimal,
    intrinsic_sol: Option<Decimal>,
    market_sol: Option<Decimal>,
    class: AssetClass,
) -> Decimal {
    if intrinsic.is_zero() {
        return Decimal::ZERO;
    }
    if matches!(class, AssetClass::Lst) {
        if let (Some(i_sol), Some(m_sol)) = (intrinsic_sol, market_sol) {
            if !i_sol.is_zero() {
                return Decimal::ONE - (m_sol / i_sol);
            }
        }
    }
    Decimal::ONE - (market / intrinsic)
}

/// Plausibility filter for raw discount samples.
///
/// `|discount| > 1.0` would mean market trades at more than 2× or less than
/// 0× intrinsic — not economically possible for any tracked class. Filtering
/// these keeps a stale-oracle or degenerate-NAV blip from contaminating the
/// smoothed `discount` for the next 7 buckets.
///
/// Caller (engine `try_recompute`) skips the EWMA update on `false` so the
/// previous smoothed value stays put; the next sane sample resumes the
/// pipeline. No publish, no alert — we never propagate the lie.
pub fn is_plausible_discount_sample(d: Decimal) -> bool {
    d.abs() <= Decimal::ONE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn zero_intrinsic_returns_zero() {
        let d = compute_discount(
            Decimal::ZERO,
            Decimal::ONE,
            None,
            None,
            AssetClass::StableFiat,
        );
        assert_eq!(d, Decimal::ZERO);
    }

    #[test]
    fn stable_uses_usd_path() {
        let d = compute_discount(
            Decimal::from_str("1.00").unwrap(),
            Decimal::from_str("0.99").unwrap(),
            None,
            None,
            AssetClass::StableFiat,
        );
        assert_eq!(d, Decimal::from_str("0.01").unwrap());
    }

    #[test]
    fn lst_prefers_sol_path_when_both_present() {
        // USD path would say drift (multiplier race), SOL path says 0.
        let intrinsic_usd: Decimal = "111.7".parse().unwrap();
        let market_usd: Decimal = "112.57".parse().unwrap();
        let i_sol: Decimal = "1.117".parse().unwrap();
        let m_sol: Decimal = "1.117".parse().unwrap();
        let d = compute_discount(
            intrinsic_usd,
            market_usd,
            Some(i_sol),
            Some(m_sol),
            AssetClass::Lst,
        );
        assert_eq!(d, Decimal::ZERO);
    }

    #[test]
    fn lst_falls_back_to_usd_when_sol_missing() {
        let d = compute_discount(
            Decimal::from_str("1.10").unwrap(),
            Decimal::from_str("1.09").unwrap(),
            None,
            None,
            AssetClass::Lst,
        );
        assert!(
            (d - Decimal::from_str("0.0090909").unwrap()).abs()
                < Decimal::from_str("0.0001").unwrap()
        );
    }

    #[test]
    fn plausibility_rejects_huge_samples() {
        assert!(!is_plausible_discount_sample(
            Decimal::from_str("-1160.47").unwrap()
        ));
        assert!(!is_plausible_discount_sample(
            Decimal::from_str("2.5").unwrap()
        ));
        assert!(is_plausible_discount_sample(
            Decimal::from_str("0.0024").unwrap()
        ));
        assert!(is_plausible_discount_sample(
            Decimal::from_str("-1").unwrap()
        ));
        assert!(is_plausible_discount_sample(Decimal::ONE));
    }
}
