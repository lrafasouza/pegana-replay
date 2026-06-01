//! Threshold resolution per asset class.

use pegana_common_verify::{AssetClass, PegState};
use rust_decimal::Decimal;
use std::collections::HashMap;

/// Yield-bearing wrappers (USDY/sUSD/syrupUSDC/sUSDe/ONyc/pbUSDC) only
/// care about the discount side — market < NAV is the redemption-stress
/// signal. Market > NAV ("premium") is just thin secondary liquidity and
/// shouldn't burn a notification.
pub fn is_direction_sensitive(class: AssetClass) -> bool {
    matches!(class, AssetClass::StableYield)
}

/// Direction-aware variant of `state_for_bps_discount`. For classes where
/// only one side of the spread carries information (currently just
/// `stable_yield`), a premium (negative discount) is normalized to PEGGED.
/// For symmetric classes the function delegates to the abs() form.
pub fn state_for_bps_discount_aware(
    class: AssetClass,
    discount: Decimal,
    thresholds: &HashMap<String, u32>,
) -> PegState {
    if is_direction_sensitive(class) && discount < Decimal::ZERO {
        return PegState::Pegged;
    }
    state_for_bps_discount(discount, thresholds)
}

/// Convert smoothed discount → state using BPS thresholds.
/// `discount` is `1 - market/intrinsic`. |discount| is what matters for
/// symmetric classes (LST, fiat, dn, fx, synth_lev). For yield-bearing
/// stables, use `state_for_bps_discount_aware` instead.
pub fn state_for_bps_discount(discount: Decimal, thresholds: &HashMap<String, u32>) -> PegState {
    let abs_bps = (discount.abs() * Decimal::from(10_000u32)).to_string();
    // Parse to integer.
    let abs_bps: u32 = abs_bps
        .split('.')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // Accept BOTH unsuffixed (assets.toml: `drift = 20`) and suffixed
    // (DB seed JSONB: `"drift_bps": 20`) keys. assets.toml is the canonical
    // source loaded at engine boot; the suffixed fallback exists so a future
    // change to read thresholds straight from the DB row would still work.
    let drift = thresholds
        .get("drift")
        .or_else(|| thresholds.get("drift_bps"))
        .copied()
        .unwrap_or(20);
    let depeg = thresholds
        .get("depeg")
        .or_else(|| thresholds.get("depeg_bps"))
        .copied()
        .unwrap_or(100);
    let critical = thresholds
        .get("critical")
        .or_else(|| thresholds.get("critical_bps"))
        .copied()
        .unwrap_or(300);

    match abs_bps {
        n if n >= critical => PegState::Critical,
        n if n >= depeg => PegState::Depeg,
        n if n >= drift => PegState::Drift,
        _ => PegState::Pegged,
    }
}

/// CR-based state for hyUSD-style CDP stables. Thresholds in percentage
/// points (e.g. 150 = 150%).
pub fn state_for_cr(cr: Decimal, thresholds: &HashMap<String, u32>) -> PegState {
    // Same dual-key tolerance as the bps function. assets.toml uses
    // `drift = 150` (no prefix); the DB JSONB uses `cr_drift`.
    let drift = thresholds
        .get("drift")
        .or_else(|| thresholds.get("cr_drift"))
        .copied()
        .unwrap_or(150);
    let depeg = thresholds
        .get("depeg")
        .or_else(|| thresholds.get("cr_depeg"))
        .copied()
        .unwrap_or(130);
    let critical = thresholds
        .get("critical")
        .or_else(|| thresholds.get("cr_critical"))
        .copied()
        .unwrap_or(110);
    let black_swan = thresholds
        .get("black_swan")
        .or_else(|| thresholds.get("cr_black_swan"))
        .copied()
        .unwrap_or(100);

    // Convert CR to integer percent.
    let cr_pct = (cr * Decimal::from(100u32))
        .to_string()
        .split('.')
        .next()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0) as u32;

    match cr_pct {
        n if n < black_swan => PegState::BlackSwan,
        n if n < critical => PegState::Critical,
        n if n < depeg => PegState::Depeg,
        n if n < drift => PegState::Drift,
        _ => PegState::Pegged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn bps_thresholds() -> HashMap<String, u32> {
        let mut m = HashMap::new();
        m.insert("drift_bps".into(), 20);
        m.insert("depeg_bps".into(), 100);
        m.insert("critical_bps".into(), 300);
        m
    }

    fn cr_thresholds() -> HashMap<String, u32> {
        let mut m = HashMap::new();
        m.insert("cr_drift".into(), 150);
        m.insert("cr_depeg".into(), 130);
        m.insert("cr_critical".into(), 110);
        m.insert("cr_black_swan".into(), 100);
        m
    }

    #[test]
    fn bps_pegged() {
        assert_eq!(
            state_for_bps_discount(Decimal::from_str("0.0010").unwrap(), &bps_thresholds()),
            PegState::Pegged
        );
    }

    #[test]
    fn bps_drift() {
        assert_eq!(
            state_for_bps_discount(Decimal::from_str("0.0050").unwrap(), &bps_thresholds()),
            PegState::Drift
        );
    }

    #[test]
    fn bps_depeg() {
        assert_eq!(
            state_for_bps_discount(Decimal::from_str("0.0150").unwrap(), &bps_thresholds()),
            PegState::Depeg
        );
    }

    #[test]
    fn bps_critical_negative() {
        assert_eq!(
            state_for_bps_discount(Decimal::from_str("-0.0350").unwrap(), &bps_thresholds()),
            PegState::Critical
        );
    }

    #[test]
    fn yield_class_ignores_premium_side() {
        assert_eq!(
            state_for_bps_discount_aware(
                AssetClass::StableYield,
                Decimal::from_str("-0.0200").unwrap(),
                &bps_thresholds()
            ),
            PegState::Pegged,
        );
        assert_eq!(
            state_for_bps_discount_aware(
                AssetClass::StableYield,
                Decimal::from_str("0.0200").unwrap(),
                &bps_thresholds()
            ),
            PegState::Depeg,
        );
    }

    #[test]
    fn lst_class_remains_symmetric() {
        for d in &["-0.0150", "0.0150"] {
            assert_eq!(
                state_for_bps_discount_aware(
                    AssetClass::Lst,
                    Decimal::from_str(d).unwrap(),
                    &bps_thresholds()
                ),
                PegState::Depeg,
            );
        }
    }

    #[test]
    fn cr_healthy() {
        assert_eq!(
            state_for_cr(Decimal::from(2), &cr_thresholds()),
            PegState::Pegged
        );
    }

    #[test]
    fn cr_drift() {
        assert_eq!(
            state_for_cr(Decimal::from_str("1.40").unwrap(), &cr_thresholds()),
            PegState::Drift
        );
    }

    #[test]
    fn cr_critical() {
        assert_eq!(
            state_for_cr(Decimal::from_str("1.05").unwrap(), &cr_thresholds()),
            PegState::Critical
        );
    }

    #[test]
    fn cr_black_swan() {
        assert_eq!(
            state_for_cr(Decimal::from_str("0.95").unwrap(), &cr_thresholds()),
            PegState::BlackSwan
        );
    }

    /// H2 consequence guard. A missing hyUSD collateral ratio USED to be
    /// synthesized to CR = 1.0 via `unwrap_or(Decimal::ONE)`. This pins exactly
    /// why that was a false alarm: CR = 1.0 → cr_pct = 100, which is NOT
    /// `< black_swan(100)` but IS `< critical(110)`, so it reads as Critical.
    /// The H2 fix (engine `try_recompute`) skips the tick instead of feeding
    /// this fabricated value here — if anyone reverts that, this is the state a
    /// silent CR read-failure would have published.
    #[test]
    fn cr_one_reads_as_critical_h2_consequence() {
        assert_eq!(
            state_for_cr(Decimal::ONE, &cr_thresholds()),
            PegState::Critical,
            "synthesized CR=1.0 fires a FALSE Critical — H2 skips the tick instead",
        );
    }
}
