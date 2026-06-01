//! Receipt and frozen-input types for audit persistence.

use chrono::{DateTime, Utc};
use pegana_common_verify::{AssetClass, PegState};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythEntry {
    pub price: Decimal,
    pub confidence: Decimal,
    pub publish_time: DateTime<Utc>,
}

/// Frozen inputs captured at decision time and hashed into the receipt.
///
/// INVARIANT — NO FLOATS IN THE RECEIPT. Every numeric field here (and in
/// [`Computed`] / [`PythEntry`]) MUST be `rust_decimal::Decimal` /
/// `Option<Decimal>`, never `f64`/`f32`. The receipt is canonicalised (RFC 8785
/// JCS, see `canonical.rs`) then sha256'd; an `f64` serialises with
/// platform/locale-dependent shortest-round-trip formatting, so the *same* value
/// can hash differently across builds/arches. A divergent hash is then read as
/// "receipt does not match its inputs" and the alert is silently suppressed —
/// the exact missing-data failure mode ADR-0019 exists to prevent. Pinned by
/// `canonical::tests::receipt_hash_is_deterministic`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputsFrozen {
    pub asset: String,
    pub class: AssetClass,
    pub now: DateTime<Utc>,
    pub alpha: Decimal,
    pub intrinsic_usd: Decimal,
    pub market_usd: Decimal,
    pub intrinsic_sol: Option<Decimal>,
    pub market_sol: Option<Decimal>,
    pub ewma_prev: Option<Decimal>,
    pub hyusd_cr: Option<Decimal>,
    pub previous_state: PegState,
    pub candidate_state: Option<PegState>,
    pub candidate_since: Option<DateTime<Utc>>,
    pub thresholds: HashMap<String, u32>,
    pub threshold_kind: String,
    pub pyth_entries: HashMap<String, PythEntry>,
    pub confirm_up_secs: i64,
    pub decay_down_secs: i64,
}

/// Engine output frozen into the receipt. Same NO-FLOATS invariant as
/// [`InputsFrozen`]: `discount_*` are `Decimal`, never `f64`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Computed {
    pub discount_raw: Decimal,
    pub discount_smooth: Decimal,
    pub final_state: PegState,
    pub confidence_label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub schema_version: String,
    pub methodology_version: String,
    pub methodology_git_sha: Option<String>,
    pub assets_toml_canonical: String,
    pub inputs_frozen: InputsFrozen,
    pub expected_computed: Computed,
    pub expected_receipt_sha256: String,
}
