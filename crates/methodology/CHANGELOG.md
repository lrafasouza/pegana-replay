# pegana-methodology changelog

## 0.4.0 — 2026-06-15

Behavior change (MINOR) — the "validation follow-ups" cluster (ADR-0025).
Additive publish-time honesty guards; no existing band thresholds change.

- **NAV-sanity premium cap** — new `premium_sanity_violated(class, discount)` +
  `NAV_PREMIUM_SANITY_BPS = 1000`. A premium (negative discount) beyond 10% on a
  direction-sensitive class (LST, stable_yield) is not demand pressure — it means
  the NAV/intrinsic anchor is broken. The ADR-0021 premium→PEGGED carve-out would
  otherwise mask it as a confident PEGGED off a garbage anchor (sHYUSD: market
  ≈ +30% over a thin-Jupiter NAV print). The engine now publishes honest-dark
  UNKNOWN instead. Magnitude check on one smoothed sample — explicitly NOT the
  discount-constancy freeze detector rejected in ADR-0024. >6× margin over the
  widest legitimate premium observed across the 26 assets (INF ≈160 bps).
- **Confidence gate** — an asset with NO Pyth feed anywhere (neither a
  `pyth_spot`/`jupiter_usd`-numeraire market nor a `pyth_*` intrinsic) has no
  independent oracle cross-check, so it can no longer assert `high` confidence:
  the engine's `pyth_confidence_for_asset` caps it at `medium`. Blast radius
  today is exactly **sUSD** (DexScreener ~$8k pool + custom rate); every other
  active asset has a Pyth dependency and is unchanged. Answers the validation
  grill "why does sUSD carry HIGH off a thin single source?". (Display-only —
  confidence is not a band threshold; no state-classification or calibration
  impact.)
- **BLACK_SWAN reachable from spread** — `state_for_bps_discount` gains a
  black_swan band, default `2 × critical` (overridable via `black_swan` /
  `black_swan_bps`). Previously the bps path topped out at CRITICAL and only the
  CR path (hyUSD) could reach BLACK_SWAN, so the publicly-advertised 5th state
  was unreachable for 25/26 assets — the validation "Four/Five states" honesty
  gap. Now a >2×-critical move (a USDC 4%+ break, a UST-style CRITICAL→BLACK_SWAN
  cascade) is labelled terminal-grade. Re-labels only the most extreme moves
  (already CRITICAL today); nothing at/below critical changes. It auto-exits via
  normal hysteresis — the engine does NOT enforce a "never auto-exits /
  operator-reset" terminal state (no reset path exists to make that safe), and
  the CR-path black_swan already behaved this way. See ADR-0025.

## 0.3.0 — 2026-06-04

Behavior change (MINOR). Activated immediately under the ADR-0009 **critical-bug
exception** ("wrong alerts firing"): during the calibration window hyUSD flapped
PEGGED↔DRIFT ~52× in two days as its collateral ratio oscillated around the
130% drift band (oracle jitter on the SOL-priced reserves clipping the
threshold). The CR classification path carried neither the EWMA smoothing nor
the magnitude deadband the spread path already had (ADR-0021).

- **CR magnitude hysteresis (Schmitt-trigger deadband)** — new
  `classify_cr_with_hysteresis`. The CR analog of `classify_with_hysteresis`
  with the band inverted (for a collateral ratio, a LOWER value is worse). A
  worsening CR escalates at the normal threshold; a relaxation toward a looser
  state only commits once CR rises above `threshold × (1 + deadband_pct)`
  (engine default 2%). **Why:** time-hysteresis (`transition.rs`
  confirm_up/decay_down) alone could not stop sustained oscillation *at* the
  boundary. Measured on the calibration window, the deadband cut hyUSD's
  transitions ~80% (52 → ~10) while keeping escalation immediate.

Receipt schema: v1 (unchanged). Verdicts change for some valid hyUSD inputs,
hence the MINOR bump; replay stays deterministic via the recorded
`methodology_git_sha`. See ADR-0023. v0.2.0 → `deprecated` (still valid for
replay), superseded by 0.3.0.

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
