//! pegana-methodology — pure functions that turn raw inputs into peg-state decisions.
//!
//! Every function is deterministic and side-effect free. Engine calls them at
//! runtime; pegana-replay-cli calls the same functions offline to verify alerts.
//! No tokio, no I/O, no global state.

pub mod canonical;
pub mod discount;
pub mod ewma;
pub mod receipt;
pub mod thresholds;
pub mod transition;
pub mod version;

pub use canonical::{canonical_assets_hash, canonical_receipt_hash, hex_sha256, MethodologyError};
pub use discount::{compute_discount, is_plausible_discount_sample};
pub use ewma::apply_ewma_pure;
pub use receipt::{Computed, InputsFrozen, PythEntry, Receipt};
pub use thresholds::{
    classify_with_hysteresis, is_direction_sensitive, state_for_bps_discount,
    state_for_bps_discount_aware, state_for_cr,
};
pub use transition::{transition_decide, TransitionDecision};
pub use version::{methodology_git_sha, methodology_version};
