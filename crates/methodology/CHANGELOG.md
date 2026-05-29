# pegana-methodology changelog

## 0.1.0 — 2026-05-29

- Initial public release.
- Extracted pure functions from pegana-engine v0.1.0; no behavior change.
- Provides: compute_discount, apply_ewma_pure, transition_decide,
  state_for_bps_discount*, state_for_cr, is_plausible_discount_sample,
  canonical_assets_hash, canonical_receipt_hash, methodology_version,
  methodology_git_sha.
- Receipt schema: v1.
