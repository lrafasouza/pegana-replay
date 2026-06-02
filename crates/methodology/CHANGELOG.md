# pegana-methodology changelog

## 0.2.0 — 2026-06-01

Behavior change (MINOR). Activated immediately under the ADR-0009 **critical-bug
exception** ("wrong alerts firing"): v0.1.0's symmetric classification was
emitting two classes of false alerts during the calibration window —
DRIFT↔PEGGED *flapping* when a smoothed discount sat near a threshold, and a
🚨 DRIFT on a benign LST *premium* (market above redemption value).

- **Magnitude hysteresis (Schmitt-trigger deadband)** — new
  `classify_with_hysteresis`. Escalation still uses the normal threshold, but a
  relaxation toward a looser state only commits once the smoothed discount falls
  below `threshold × (1 − deadband_pct)` (engine default 25%). Time-hysteresis
  (`transition.rs` confirm_up/decay_down) alone could not stop oscillation *at*
  the boundary. **Why:** JupSOL flapped DRIFT↔PEGGED around 60 bps (e.g. 84.7 →
  48.8 bps); the deadband (exit 45 bps) holds the state through the dead zone.
- **LST premium carve-out** — `is_direction_sensitive` now includes
  `AssetClass::Lst`. A premium (market > intrinsic) normalizes to PEGGED, same
  as yield-bearing stables. **Why:** for an LST the risk signal is the *discount*
  (redemption stress, cf. stETH −7% in 2022, ezETH depeg); a premium is demand
  pressure, not stress. Discount-side classification is unchanged.

Receipt schema: v1 (unchanged). Verdicts change for some valid inputs, hence the
MINOR bump; replay stays deterministic via the recorded `methodology_git_sha`.
See ADR-0021. v0.1.0 → `deprecated` (still valid for replay), superseded by 0.2.0.

## 0.1.0 — 2026-05-29

- Initial public release.
- Extracted pure functions from pegana-engine v0.1.0; no behavior change.
- Provides: compute_discount, apply_ewma_pure, transition_decide,
  state_for_bps_discount*, state_for_cr, is_plausible_discount_sample,
  canonical_assets_hash, canonical_receipt_hash, methodology_version,
  methodology_git_sha.
- Receipt schema: v1.
