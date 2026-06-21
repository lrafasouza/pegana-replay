//! Offline re-derivation of the peg-state verdict from frozen receipt inputs.
//!
//! `rederive` replicates the engine `try_recompute` pure pipeline EXACTLY so
//! that anyone can verify "the frozen inputs actually produce the recorded
//! verdict" without running the engine. This is the Grant Thesis ("anyone can
//! recompute our peg history") operationalized.
//!
//! # What is re-derived
//!
//! - `discount_raw` — compute_discount from frozen intrinsic/market prices.
//! - `discount_smooth` — one EWMA step from frozen `ewma_prev` + frozen `alpha`.
//! - `final_state` — classify then transition then premium-sanity override.
//!
//! # What is NOT re-derived (intentionally deferred)
//!
//! `confidence_label` is intentionally excluded. Its engine resolver reads
//! `Utc::now()` to gate Pyth feed staleness, making it non-reproducible offline.
//! Option-B follow-up: move the label computation fully into methodology so it
//! can take an explicit `now` — at that point it can be frozen and verified here.
//! Until then, only `(discount_raw, discount_smooth, final_state)` are verified.
//!
//! # Schema v2
//!
//! `rederive` only applies to schema v2 receipts. v1 receipts were emitted with
//! capture-ordering bugs (GAP-1/2/3) that mean `ewma_prev`, `candidate_state`,
//! `candidate_since`, and `previous_state` were not correctly frozen, so
//! re-derivation would produce spurious mismatches. The CLI skips re-derivation
//! for v1 receipts (re-hash only — the existing tamper-evidence path) and
//! applies it only to v2+.

use crate::{
    apply_ewma_pure, classify_cr_with_hysteresis, classify_with_hysteresis, compute_discount,
    is_plausible_discount_sample, premium_sanity_violated, receipt::InputsFrozen,
    transition_decide, CR_DEADBAND_PCT, DEADBAND_PCT,
};
use pegana_common_verify::PegState;
use rust_decimal::Decimal;
use thiserror::Error;

/// Error variants for `rederive`.
#[derive(Debug, Error, PartialEq)]
pub enum MethodologyRederiveError {
    /// `compute_discount` returned `None` — intrinsic is zero or produces an
    /// overflow. A v2 receipt is only emitted when discount was `Some`, so this
    /// indicates inconsistent frozen inputs.
    #[error("intrinsic zero or overflow — discount cannot be re-derived")]
    IntrinsicZeroOrOverflow,

    /// The frozen `discount_raw` is outside `|d| < 1.0`. The engine returns
    /// early (no receipt) on implausible samples, so a v2 receipt should never
    /// carry one.
    #[error("implausible discount frozen in receipt — engine would have skipped this tick")]
    ImplausibleSample,

    /// hyUSD CR path requires `hyusd_cr` but the field is `None`.
    #[error("hyUSD CR path selected but hyusd_cr is None in frozen inputs")]
    MissingHyusdCr,
}

/// The re-derived verdict from frozen `InputsFrozen`.
///
/// Covers the three deterministic outputs: the raw discount, the EWMA-smoothed
/// discount, and the final peg state after classification, time-hysteresis, and
/// the premium-sanity override. `confidence_label` is intentionally absent — see
/// module-level docs.
#[derive(Debug, Clone, PartialEq)]
pub struct Rederived {
    pub discount_raw: Decimal,
    pub discount_smooth: Decimal,
    pub final_state: PegState,
}

/// Re-derive the peg-state verdict from frozen receipt inputs.
///
/// Replicates the engine `try_recompute` pure pipeline in the exact order:
/// 1. `compute_discount` → `discount_raw`
/// 2. `is_plausible_discount_sample` guard
/// 3. `apply_ewma_pure` → `discount_smooth`
/// 4. `classify_with_hysteresis` / `classify_cr_with_hysteresis` → `candidate`
/// 5. `transition_decide` → `final_state`
/// 6. `premium_sanity_violated` override → `Unknown`
///
/// # GAP-3 / `previous_state` semantics
///
/// `inputs.previous_state` must be the engine's `g.last_state` captured BEFORE
/// `transition` mutated it (schema v2 guarantee). It is used as `current` for
/// BOTH `classify_*` (deadband seed) and `transition_decide`
/// (`current_last_state`). In steady-state `g.last_state == last_published_state`
/// so both agree. On cold-start `g.last_state` defaults to `Pegged`; v1 receipts
/// froze `prev_opt.unwrap_or(Unknown)` (= `Unknown`) instead — that is the
/// GAP-3 divergence that made re-derivation wrong on cold-start for v1.
///
/// # hyUSD CR branch condition
///
/// The engine branches on `matches!(class, AssetClass::StableCdp) && asset == "hyUSD"`,
/// which corresponds exactly to `threshold_kind == "cr"` in the frozen inputs.
/// We use `threshold_kind` for the branch here so the CLI has zero hard-coded
/// asset-name logic; the field is always `"cr"` for hyUSD and `"bps"` otherwise.
pub fn rederive(inputs: &InputsFrozen) -> Result<Rederived, MethodologyRederiveError> {
    // Step 1: compute raw discount.
    let discount_raw = compute_discount(
        inputs.intrinsic_usd,
        inputs.market_usd,
        inputs.intrinsic_sol,
        inputs.market_sol,
        inputs.class,
    )
    .ok_or(MethodologyRederiveError::IntrinsicZeroOrOverflow)?;

    // Step 2: plausibility guard. The engine returns early (no receipt) when
    // the sample is implausible; a v2 receipt carrying one is inconsistent.
    if !is_plausible_discount_sample(discount_raw) {
        return Err(MethodologyRederiveError::ImplausibleSample);
    }

    // Step 3: EWMA smoothing. `apply_ewma_pure` seeds at `raw` when `prev` is
    // None (cold-start: no prior EWMA for this asset).
    let discount_smooth = apply_ewma_pure(discount_raw, inputs.ewma_prev, inputs.alpha);

    // Step 4: classify. Branch on `threshold_kind` — exactly mirrors the engine
    // condition `matches!(class, AssetClass::StableCdp) && asset == "hyUSD"`.
    // `inputs.previous_state` is the frozen `g.last_state` (schema v2) and
    // serves as the deadband seed (`current`) for both classify functions.
    let candidate = if inputs.threshold_kind == "cr" {
        let cr = inputs
            .hyusd_cr
            .ok_or(MethodologyRederiveError::MissingHyusdCr)?;
        classify_cr_with_hysteresis(
            cr,
            &inputs.thresholds,
            inputs.previous_state,
            CR_DEADBAND_PCT,
        )
    } else {
        classify_with_hysteresis(
            inputs.class,
            discount_smooth,
            &inputs.thresholds,
            inputs.previous_state,
            DEADBAND_PCT,
        )
    };

    // Step 5: time-hysteresis (transition_decide). Uses `inputs.previous_state`
    // as `current_last_state` — same field that seeds classify above.
    // `candidate_state`/`candidate_since` are the frozen pre-transition values
    // (schema v2 guarantee via GAP-2 fix).
    let decision = transition_decide(
        inputs.previous_state,
        candidate,
        inputs.candidate_state,
        inputs.candidate_since,
        inputs.now,
        inputs.confirm_up_secs,
        inputs.decay_down_secs,
    );
    let mut final_state = decision.new_last_state;

    // Step 6: premium-sanity override. Mirrors the `premium_sanity_violated`
    // block in `try_recompute` (after `transition`, same engine ordering):
    // `if premium_sanity_violated(asset_cfg.class, new_smooth) { final_state = Unknown }`.
    if premium_sanity_violated(inputs.class, discount_smooth) {
        final_state = PegState::Unknown;
    }

    Ok(Rederived {
        discount_raw,
        discount_smooth,
        final_state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt::InputsFrozen;
    use chrono::{DateTime, Duration, Utc};
    use pegana_common_verify::{AssetClass, PegState};
    use rust_decimal::Decimal;
    use std::collections::HashMap;
    use std::str::FromStr;

    fn fixed_now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_704_067_200, 0).unwrap()
    }

    fn bps_thresholds() -> HashMap<String, u32> {
        let mut m = HashMap::new();
        m.insert("drift".into(), 20);
        m.insert("depeg".into(), 100);
        m.insert("critical".into(), 300);
        m
    }

    fn cr_thresholds() -> HashMap<String, u32> {
        let mut m = HashMap::new();
        m.insert("drift".into(), 150);
        m.insert("depeg".into(), 130);
        m.insert("critical".into(), 110);
        m.insert("black_swan".into(), 100);
        m
    }

    fn base_inputs(class: AssetClass, threshold_kind: &str) -> InputsFrozen {
        InputsFrozen {
            asset: "TEST".into(),
            class,
            now: fixed_now(),
            alpha: Decimal::from_str("0.3").unwrap(),
            intrinsic_usd: Decimal::ONE,
            market_usd: Decimal::ONE,
            intrinsic_sol: None,
            market_sol: None,
            ewma_prev: None,
            hyusd_cr: None,
            previous_state: PegState::Pegged,
            candidate_state: None,
            candidate_since: None,
            thresholds: bps_thresholds(),
            threshold_kind: threshold_kind.into(),
            pyth_entries: HashMap::new(),
            confirm_up_secs: 30,
            decay_down_secs: 120,
        }
    }

    // (a) USD stable PEGGED — market at intrinsic, previous PEGGED, no EWMA prior.
    #[test]
    fn usd_stable_pegged() {
        let inputs = base_inputs(AssetClass::StableFiat, "bps");
        let r = rederive(&inputs).expect("should succeed");
        assert_eq!(r.discount_raw, Decimal::ZERO);
        assert_eq!(r.discount_smooth, Decimal::ZERO); // cold-start seeds at raw
        assert_eq!(r.final_state, PegState::Pegged);
    }

    // (b) USD stable crossing into DRIFT — 30bps discount, previous PEGGED.
    // 30bps > drift(20bps) → candidate = DRIFT. Timer just started (new candidate,
    // no prior) → transition_decide returns current (Pegged) with candidate queued.
    // So final_state remains PEGGED until confirm_up elapses.
    #[test]
    fn usd_stable_enters_drift_first_tick() {
        let mut inputs = base_inputs(AssetClass::StableFiat, "bps");
        // market = 0.997, intrinsic = 1.0 → discount = 0.003 = 30bps > 20bps drift
        inputs.market_usd = Decimal::from_str("0.997").unwrap();
        let r = rederive(&inputs).expect("should succeed");
        // raw discount = 1 - 0.997/1.0 = 0.003
        assert_eq!(r.discount_raw, Decimal::from_str("0.003").unwrap());
        // cold-start EWMA seeds at raw
        assert_eq!(r.discount_smooth, Decimal::from_str("0.003").unwrap());
        // New candidate DRIFT, timer just started → Pegged stays (Case 2)
        assert_eq!(r.final_state, PegState::Pegged);
        assert_eq!(inputs.candidate_state, None); // frozen before transition
    }

    // (b2) Confirm DRIFT after timer: candidate already queued for > confirm_up_secs.
    #[test]
    fn usd_stable_commits_drift_after_confirm_window() {
        let mut inputs = base_inputs(AssetClass::StableFiat, "bps");
        inputs.market_usd = Decimal::from_str("0.997").unwrap();
        // Fake: candidate already queued as DRIFT 31s before now → matures.
        inputs.candidate_state = Some(PegState::Drift);
        inputs.candidate_since = Some(fixed_now() - Duration::seconds(31));
        let r = rederive(&inputs).expect("should succeed");
        assert_eq!(r.final_state, PegState::Drift);
    }

    // (c) LST using the SOL path.
    #[test]
    fn lst_sol_path() {
        let mut inputs = base_inputs(AssetClass::Lst, "bps");
        // SOL-denominated: intrinsic_sol = 1.0, market_sol = 1.0 → discount = 0
        inputs.intrinsic_sol = Some(Decimal::from_str("1.0").unwrap());
        inputs.market_sol = Some(Decimal::from_str("1.0").unwrap());
        // Make USD prices differ to confirm we are on the SOL path
        inputs.intrinsic_usd = Decimal::from_str("100.0").unwrap();
        inputs.market_usd = Decimal::from_str("101.0").unwrap(); // USD premium but SOL = 0
        let r = rederive(&inputs).expect("should succeed");
        assert_eq!(r.discount_raw, Decimal::ZERO);
        assert_eq!(r.final_state, PegState::Pegged);
    }

    // (d) hyUSD CR path: StableCdp + threshold_kind="cr" + hyusd_cr present.
    // CR = 1.60 (above drift=150) → PEGGED.
    #[test]
    fn hyusd_cr_path_pegged() {
        let mut inputs = base_inputs(AssetClass::StableCdp, "cr");
        inputs.thresholds = cr_thresholds();
        inputs.hyusd_cr = Some(Decimal::from_str("1.60").unwrap());
        // intrinsic/market can be anything reasonable for the discount path;
        // the CR path uses hyusd_cr for classify but still requires a valid discount.
        inputs.intrinsic_usd = Decimal::from_str("1.0").unwrap();
        inputs.market_usd = Decimal::from_str("0.999").unwrap();
        let r = rederive(&inputs).expect("should succeed");
        assert_eq!(r.final_state, PegState::Pegged);
    }

    // (d2) hyUSD CR path: CR = 1.40 (130 < 140 < 150 = drift threshold) → DRIFT.
    // First tick with new candidate → timer starts, stays Pegged.
    #[test]
    fn hyusd_cr_path_drift_first_tick() {
        let mut inputs = base_inputs(AssetClass::StableCdp, "cr");
        inputs.thresholds = cr_thresholds();
        inputs.hyusd_cr = Some(Decimal::from_str("1.40").unwrap());
        inputs.intrinsic_usd = Decimal::from_str("1.0").unwrap();
        inputs.market_usd = Decimal::from_str("0.999").unwrap();
        let r = rederive(&inputs).expect("should succeed");
        // classify_cr → DRIFT (1.40 < 1.50), transition → Pegged (Case 2, timer started)
        assert_eq!(r.final_state, PegState::Pegged);
    }

    // (d3) hyUSD CR path committed after confirm window.
    #[test]
    fn hyusd_cr_path_drift_committed() {
        let mut inputs = base_inputs(AssetClass::StableCdp, "cr");
        inputs.thresholds = cr_thresholds();
        inputs.hyusd_cr = Some(Decimal::from_str("1.40").unwrap());
        inputs.intrinsic_usd = Decimal::from_str("1.0").unwrap();
        inputs.market_usd = Decimal::from_str("0.999").unwrap();
        inputs.candidate_state = Some(PegState::Drift);
        inputs.candidate_since = Some(fixed_now() - Duration::seconds(31));
        let r = rederive(&inputs).expect("should succeed");
        assert_eq!(r.final_state, PegState::Drift);
    }

    // (e) Premium-sanity override: LST with > 10% premium → Unknown.
    #[test]
    fn premium_sanity_override_to_unknown() {
        let mut inputs = base_inputs(AssetClass::Lst, "bps");
        // discount = 1 - 1.15/1.0 = -0.15 (-1500bps) → premium 15% > 10% → Unknown
        inputs.intrinsic_usd = Decimal::from_str("1.0").unwrap();
        inputs.market_usd = Decimal::from_str("1.15").unwrap();
        // No SOL override → USD path
        inputs.intrinsic_sol = None;
        inputs.market_sol = None;
        let r = rederive(&inputs).expect("should succeed");
        assert_eq!(r.final_state, PegState::Unknown);
    }

    // (f) Cold-start: previous_state = Pegged (default), candidate_state = None.
    // Verifies clean seeding without any prior EWMA.
    #[test]
    fn cold_start_pegged_no_ewma() {
        let inputs = base_inputs(AssetClass::StableFiat, "bps");
        // previous_state = Pegged, candidate_state = None, ewma_prev = None
        assert_eq!(inputs.previous_state, PegState::Pegged);
        assert_eq!(inputs.candidate_state, None);
        assert_eq!(inputs.ewma_prev, None);
        let r = rederive(&inputs).expect("cold-start pegged succeeds");
        assert_eq!(r.discount_raw, Decimal::ZERO);
        assert_eq!(r.discount_smooth, Decimal::ZERO); // seeds at raw
        assert_eq!(r.final_state, PegState::Pegged);
    }

    // Error case: zero intrinsic.
    #[test]
    fn zero_intrinsic_errors() {
        let mut inputs = base_inputs(AssetClass::StableFiat, "bps");
        inputs.intrinsic_usd = Decimal::ZERO;
        assert_eq!(
            rederive(&inputs),
            Err(MethodologyRederiveError::IntrinsicZeroOrOverflow)
        );
    }

    // Error case: implausible sample (market = 0 → discount = 1.0 = not plausible).
    #[test]
    fn implausible_sample_errors() {
        let mut inputs = base_inputs(AssetClass::StableFiat, "bps");
        inputs.market_usd = Decimal::ZERO; // discount = 1.0, not plausible
        assert_eq!(
            rederive(&inputs),
            Err(MethodologyRederiveError::ImplausibleSample)
        );
    }

    // Error case: CR path but hyusd_cr missing.
    #[test]
    fn cr_path_missing_cr_errors() {
        let mut inputs = base_inputs(AssetClass::StableCdp, "cr");
        inputs.thresholds = cr_thresholds();
        inputs.hyusd_cr = None; // missing
        assert_eq!(
            rederive(&inputs),
            Err(MethodologyRederiveError::MissingHyusdCr)
        );
    }

    // EWMA: with a prior value.
    #[test]
    fn ewma_blends_with_prior() {
        let mut inputs = base_inputs(AssetClass::StableFiat, "bps");
        // discount_raw = 0.002 (20bps, at the drift threshold = 20)
        inputs.market_usd = Decimal::from_str("0.998").unwrap();
        inputs.ewma_prev = Some(Decimal::from_str("0.001").unwrap());
        // smooth = 0.3 * 0.002 + 0.7 * 0.001 = 0.0006 + 0.0007 = 0.0013
        let r = rederive(&inputs).expect("should succeed");
        assert_eq!(r.discount_raw, Decimal::from_str("0.002").unwrap());
        let expected_smooth = Decimal::from_str("0.0013").unwrap();
        assert_eq!(r.discount_smooth, expected_smooth);
    }
}
