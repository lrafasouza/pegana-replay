//! Threshold resolution per asset class.

use pegana_common_verify::{AssetClass, PegState};
use rust_decimal::Decimal;
use std::collections::HashMap;

/// Classes where only the DISCOUNT side (market < intrinsic/NAV) carries a
/// risk signal:
///   - Yield-bearing stables (USDY/sUSD/syrupUSDC/sUSDe/ONyc/pbUSDC): market <
///     NAV is the redemption-stress signal; market > NAV is thin secondary
///     liquidity.
///   - LSTs (JupSOL/jitoSOL/mSOL/…): a *premium* (market > redemption value)
///     is demand pressure, not stress — holders can always redeem at intrinsic.
///     The dangerous deviation is the *discount* (cf. stETH −7% in 2022, ezETH
///     depeg) where sellers outrun arbitrage. So a premium normalizes to
///     PEGGED instead of burning a 🚨 DRIFT.
pub fn is_direction_sensitive(class: AssetClass) -> bool {
    matches!(class, AssetClass::StableYield | AssetClass::Lst)
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

/// Strictness rank for hysteresis comparisons (mirrors the engine
/// `transition.rs` ordering). Higher = more severe.
fn rank(s: PegState) -> u8 {
    match s {
        PegState::Pegged | PegState::Unknown => 0,
        PegState::Drift => 1,
        PegState::Depeg => 2,
        PegState::Critical => 3,
        PegState::BlackSwan => 4,
    }
}

/// Lower every band threshold by `deadband_pct` percent (integer, floored).
/// Used to build the *exit* thresholds for the Schmitt-trigger band.
fn lower_thresholds(thresholds: &HashMap<String, u32>, deadband_pct: u32) -> HashMap<String, u32> {
    let keep = 100u32.saturating_sub(deadband_pct);
    thresholds
        .iter()
        .map(|(k, v)| (k.clone(), v.saturating_mul(keep) / 100))
        .collect()
}

/// Magnitude-hysteresis (Schmitt-trigger) classification layer over
/// `state_for_bps_discount_aware`. Time-based hysteresis (engine
/// `transition.rs` confirm_up/decay_down) suppresses brief spikes but NOT a
/// signal that sits and oscillates *around* a threshold — that flaps
/// (JupSOL: DRIFT↔PEGGED around 60bps). A deadband fixes it: escalate to a
/// stricter state at the normal threshold, but only relax back once the
/// discount falls below `threshold × (1 - deadband_pct%)`. `deadband_pct = 0`
/// reduces to the plain `aware` classification.
pub fn classify_with_hysteresis(
    class: AssetClass,
    discount: Decimal,
    thresholds: &HashMap<String, u32>,
    current: PegState,
    deadband_pct: u32,
) -> PegState {
    let raw = state_for_bps_discount_aware(class, discount, thresholds);
    // Escalation (or no change): react at the normal threshold — never let the
    // deadband slow down a worsening peg. Time-hysteresis (confirm_up_secs)
    // already debounces the way up.
    if rank(raw) >= rank(current) {
        return raw;
    }
    // Relaxation toward a looser state: only allow it once the discount falls
    // below the deadband-lowered (exit) thresholds. Classifying against the
    // lowered thresholds yields a state between `raw` and `current` — the band
    // "sticks" until the signal clearly exits.
    let exit_thresholds = lower_thresholds(thresholds, deadband_pct);
    state_for_bps_discount_aware(class, discount, &exit_thresholds)
}

/// Raise every band threshold by `deadband_pct` percent (integer, floored).
/// The CR analog of `lower_thresholds`: for CR-driven assets a LOWER ratio is
/// worse, so the Schmitt-trigger *exit* band sits ABOVE the entry thresholds.
fn raise_thresholds(thresholds: &HashMap<String, u32>, deadband_pct: u32) -> HashMap<String, u32> {
    thresholds
        .iter()
        .map(|(k, v)| (k.clone(), v.saturating_mul(100 + deadband_pct) / 100))
        .collect()
}

/// CR magnitude-hysteresis (Schmitt-trigger) over `state_for_cr`. A CR-driven
/// asset (hyUSD) whose collateral ratio sits a few points above its drift band
/// flaps PEGGED↔DRIFT as oracle jitter clips the threshold. Time-hysteresis
/// (engine `transition.rs` confirm_up/decay_down) debounces brief spikes but
/// NOT sustained oscillation around the band. A deadband fixes it: escalate
/// (CR dropping = worse) at the normal threshold, but only relax once CR rises
/// above `threshold × (1 + deadband_pct%)`. `deadband_pct = 0` reduces to plain
/// `state_for_cr`. Mirrors `classify_with_hysteresis` (spread) with the band
/// inverted, since for CR lower = worse.
pub fn classify_cr_with_hysteresis(
    cr: Decimal,
    thresholds: &HashMap<String, u32>,
    current: PegState,
    deadband_pct: u32,
) -> PegState {
    let raw = state_for_cr(cr, thresholds);
    // Escalation (or no change): react at the normal threshold — never let the
    // deadband slow down a worsening (dropping-CR) peg.
    if rank(raw) >= rank(current) {
        return raw;
    }
    // Relaxation (rising CR): only allow it once CR clears the deadband-raised
    // (exit) thresholds, so a CR oscillating just above its band sticks instead
    // of flapping back to PEGGED.
    let exit_thresholds = raise_thresholds(thresholds, deadband_pct);
    state_for_cr(cr, &exit_thresholds)
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

/// The next-worse band below `current` for a CR-driven asset, as a display
/// label + its CR threshold (percent). `None` if already at the worst band
/// (BlackSwan) or not in an alerting band (Pegged/Unknown). Same dual-key
/// tolerance as `state_for_cr` (assets.toml `depeg` / DB JSONB `cr_depeg`).
pub fn next_worse_cr_band(
    current: PegState,
    thresholds: &HashMap<String, u32>,
) -> Option<(&'static str, u32)> {
    let get = |k: &str, alt: &str, d: u32| {
        thresholds
            .get(k)
            .or_else(|| thresholds.get(alt))
            .copied()
            .unwrap_or(d)
    };
    match current {
        PegState::Drift => Some(("DEPEG", get("depeg", "cr_depeg", 130))),
        PegState::Depeg => Some(("CRITICAL", get("critical", "cr_critical", 110))),
        PegState::Critical => Some(("BLACK_SWAN", get("black_swan", "cr_black_swan", 100))),
        _ => None,
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
    fn cr_hysteresis_escalates_immediately() {
        let t = cr_thresholds(); // drift=150
                                 // CR dropping below drift fires DRIFT at once, even with a deadband.
        assert_eq!(
            classify_cr_with_hysteresis(
                Decimal::from_str("1.49").unwrap(),
                &t,
                PegState::Pegged,
                2
            ),
            PegState::Drift
        );
        // A deeper drop escalates straight to DEPEG — never slow a worsening peg.
        assert_eq!(
            classify_cr_with_hysteresis(Decimal::from_str("1.29").unwrap(), &t, PegState::Drift, 2),
            PegState::Depeg
        );
    }

    #[test]
    fn cr_hysteresis_holds_inside_the_deadband() {
        let t = cr_thresholds(); // drift=150; 2% exit band = 153
                                 // CR recovered above drift (151 >= 150) but not above the 153 exit band:
                                 // stay DRIFT instead of flapping back to PEGGED.
        assert_eq!(
            classify_cr_with_hysteresis(Decimal::from_str("1.51").unwrap(), &t, PegState::Drift, 2),
            PegState::Drift
        );
    }

    #[test]
    fn cr_hysteresis_relaxes_once_cr_clears_the_band() {
        let t = cr_thresholds(); // 2% exit band = 153
        assert_eq!(
            classify_cr_with_hysteresis(Decimal::from_str("1.53").unwrap(), &t, PegState::Drift, 2),
            PegState::Pegged
        );
    }

    #[test]
    fn cr_hysteresis_zero_deadband_is_plain_state_for_cr() {
        let t = cr_thresholds();
        // deadband_pct = 0 → relaxes at the bare threshold (151 >= 150 → PEGGED).
        assert_eq!(
            classify_cr_with_hysteresis(Decimal::from_str("1.51").unwrap(), &t, PegState::Drift, 0),
            PegState::Pegged
        );
    }

    #[test]
    fn next_worse_from_drift_is_depeg() {
        let t = cr_thresholds(); // drift=150 depeg=130 critical=110 black_swan=100
        assert_eq!(
            next_worse_cr_band(PegState::Drift, &t),
            Some(("DEPEG", 130))
        );
    }

    #[test]
    fn next_worse_from_depeg_is_critical() {
        assert_eq!(
            next_worse_cr_band(PegState::Depeg, &cr_thresholds()),
            Some(("CRITICAL", 110))
        );
    }

    #[test]
    fn next_worse_from_critical_is_black_swan() {
        assert_eq!(
            next_worse_cr_band(PegState::Critical, &cr_thresholds()),
            Some(("BLACK_SWAN", 100))
        );
    }

    #[test]
    fn next_worse_from_black_swan_or_pegged_is_none() {
        let t = cr_thresholds();
        assert_eq!(next_worse_cr_band(PegState::BlackSwan, &t), None);
        assert_eq!(next_worse_cr_band(PegState::Pegged, &t), None);
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

    // #3: an LST premium (market > intrinsic) is demand pressure, not
    // redemption stress — the risk side is the DISCOUNT (cf. stETH 2022,
    // ezETH). So LSTs join the direction-sensitive set: premium → PEGGED,
    // discount → classified normally.
    #[test]
    fn lst_ignores_premium_side() {
        // premium (negative discount) normalizes to PEGGED
        assert_eq!(
            state_for_bps_discount_aware(
                AssetClass::Lst,
                Decimal::from_str("-0.0150").unwrap(),
                &bps_thresholds()
            ),
            PegState::Pegged,
        );
        // discount side still classifies (real stress signal)
        assert_eq!(
            state_for_bps_discount_aware(
                AssetClass::Lst,
                Decimal::from_str("0.0150").unwrap(),
                &bps_thresholds()
            ),
            PegState::Depeg,
        );
    }

    // #1: Schmitt-trigger deadband. drift=60 → exit at 60×0.75=45 (25% band).
    fn jup_thresholds() -> HashMap<String, u32> {
        let mut m = HashMap::new();
        m.insert("drift".into(), 60);
        m.insert("depeg".into(), 150);
        m.insert("critical".into(), 300);
        m
    }

    #[test]
    fn hysteresis_enters_drift_at_normal_threshold() {
        // From PEGGED, escalation uses the full threshold (no deadband on the way up).
        assert_eq!(
            classify_with_hysteresis(
                AssetClass::Lst,
                Decimal::from_str("0.0065").unwrap(), // 65bps ≥ 60
                &jup_thresholds(),
                PegState::Pegged,
                25,
            ),
            PegState::Drift,
        );
    }

    #[test]
    fn hysteresis_holds_drift_inside_deadband() {
        // Already DRIFT, discount dips to 50bps: below entry (60) but above
        // exit (45) → must STAY DRIFT (this is the flap the old code emitted).
        assert_eq!(
            classify_with_hysteresis(
                AssetClass::Lst,
                Decimal::from_str("0.0050").unwrap(),
                &jup_thresholds(),
                PegState::Drift,
                25,
            ),
            PegState::Drift,
        );
    }

    #[test]
    fn hysteresis_repegs_below_exit_threshold() {
        // Already DRIFT, discount falls to 40bps (< exit 45) → repeg to PEGGED.
        assert_eq!(
            classify_with_hysteresis(
                AssetClass::Lst,
                Decimal::from_str("0.0040").unwrap(),
                &jup_thresholds(),
                PegState::Drift,
                25,
            ),
            PegState::Pegged,
        );
    }

    #[test]
    fn hysteresis_escalates_without_deadband() {
        // Deadband must NEVER slow an escalation: PEGGED → DEPEG at 160bps.
        assert_eq!(
            classify_with_hysteresis(
                AssetClass::Lst,
                Decimal::from_str("0.0160").unwrap(),
                &jup_thresholds(),
                PegState::Pegged,
                25,
            ),
            PegState::Depeg,
        );
    }

    #[test]
    fn hysteresis_steps_down_one_band_with_deadband() {
        // From DEPEG, 140bps: below depeg(150) but above depeg-exit(112) →
        // stay DEPEG.
        assert_eq!(
            classify_with_hysteresis(
                AssetClass::Lst,
                Decimal::from_str("0.0140").unwrap(),
                &jup_thresholds(),
                PegState::Depeg,
                25,
            ),
            PegState::Depeg,
        );
        // 100bps: below depeg-exit(112) → leave DEPEG; still ≥ drift-exit(45) → DRIFT.
        assert_eq!(
            classify_with_hysteresis(
                AssetClass::Lst,
                Decimal::from_str("0.0100").unwrap(),
                &jup_thresholds(),
                PegState::Depeg,
                25,
            ),
            PegState::Drift,
        );
    }

    #[test]
    fn hysteresis_zero_deadband_is_plain_classification() {
        assert_eq!(
            classify_with_hysteresis(
                AssetClass::Lst,
                Decimal::from_str("0.0050").unwrap(),
                &jup_thresholds(),
                PegState::Drift,
                0,
            ),
            PegState::Pegged, // 50 < 60, no deadband → repegs immediately
        );
    }

    #[test]
    fn hysteresis_lst_premium_stays_pegged_even_when_current_drift() {
        // A premium can't hold an LST in DRIFT — directional carve-out wins.
        assert_eq!(
            classify_with_hysteresis(
                AssetClass::Lst,
                Decimal::from_str("-0.0080").unwrap(),
                &jup_thresholds(),
                PegState::Drift,
                25,
            ),
            PegState::Pegged,
        );
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
