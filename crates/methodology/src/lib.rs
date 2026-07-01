//! pegana-methodology — pure functions that turn raw inputs into peg-state decisions.
//!
//! Every function is deterministic and side-effect free. Engine calls them at
//! runtime; pegana-replay-cli calls the same functions offline to verify alerts.
//! No tokio, no I/O, no global state.

/// Emitted by the engine when the NAV-sanity check overrides the verdict
/// (smoothed discount exceeded the premium-sanity bound; intrinsic anchor
/// suspect; state forced to UNKNOWN). Mirrors `Receipt.state_reason` and
/// `discount_snapshots.state_reason`. The API SQL uses the string literal
/// `'premium_sanity'` which must stay identical to this const.
pub const STATE_REASON_PREMIUM_SANITY: &str = "premium_sanity";

/// Synthesized by the API layer (not the engine) when the most-recent
/// discount snapshot is older than 15 minutes (stale feed). The state is
/// collapsed from the stored value to UNKNOWN. The API SQL uses the string
/// literal `'stale_source'` which must stay identical to this const.
/// Not present in engine receipts — the engine stops writing when data is
/// absent, so a stale row implies the engine saw a dead feed, not a decision.
pub const STATE_REASON_STALE_SOURCE: &str = "stale_source";

pub mod canonical;
pub mod discount;
pub mod ewma;
pub mod receipt;
pub mod rederive;
pub mod thresholds;
pub mod transition;
pub mod version;

pub use canonical::{canonical_assets_hash, canonical_receipt_hash, hex_sha256, MethodologyError};
pub use discount::{compute_discount, is_plausible_discount_sample};
pub use ewma::apply_ewma_pure;
pub use receipt::{Computed, InputsFrozen, PythEntry, Receipt};
pub use rederive::{rederive, MethodologyRederiveError, Rederived};
pub use thresholds::{
    classify_cr_with_hysteresis, classify_with_hysteresis, is_direction_sensitive,
    next_worse_cr_band, premium_sanity_violated, state_for_bps_discount,
    state_for_bps_discount_aware, state_for_cr, CR_DEADBAND_PCT, DEADBAND_PCT,
    NAV_PREMIUM_SANITY_BPS,
};
pub use transition::{transition_decide, TransitionDecision};
pub use version::{methodology_git_sha, methodology_version};
