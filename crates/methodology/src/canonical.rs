//! Canonical JSON (RFC 8785) and TOML hashing.

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::receipt::{Computed, InputsFrozen};

#[derive(Debug, Error)]
pub enum MethodologyError {
    #[error("failed to parse assets.toml: {0}")]
    AssetsTomlParse(#[from] toml::de::Error),

    #[error("failed to serialize canonical TOML: {0}")]
    AssetsTomlSerialize(#[from] toml::ser::Error),

    #[error("canonical JSON encode failed: {0}")]
    CanonicalJson(#[from] serde_json::Error),
}

/// Parse assets.toml, re-serialize in canonical form (sorted keys),
/// then sha256. Comments and whitespace don't affect the hash.
pub fn canonical_assets_hash(toml_str: &str) -> Result<[u8; 32], MethodologyError> {
    let value: toml::Value = toml::from_str(toml_str)?;
    let canonical = toml::to_string(&value)?;
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    Ok(hasher.finalize().into())
}

/// Canonical sha256 of a receipt's content via RFC 8785 JCS encoding.
pub fn canonical_receipt_hash(
    methodology_version: &str,
    methodology_git_sha: Option<&str>,
    assets_toml_canonical: &str,
    inputs_frozen: &InputsFrozen,
    computed: &Computed,
) -> Result<[u8; 32], MethodologyError> {
    let mut hasher = Sha256::new();
    hasher.update(methodology_version.as_bytes());
    hasher.update(b"\x00");
    hasher.update(methodology_git_sha.unwrap_or("").as_bytes());
    hasher.update(b"\x00");
    hasher.update(assets_toml_canonical.as_bytes());
    hasher.update(b"\x00");
    let inputs_bytes = serde_json_canonicalizer::to_string(inputs_frozen)?;
    hasher.update(inputs_bytes.as_bytes());
    hasher.update(b"\x00");
    let computed_bytes = serde_json_canonicalizer::to_string(computed)?;
    hasher.update(computed_bytes.as_bytes());
    Ok(hasher.finalize().into())
}

/// Hex-encode a sha256 digest for CHAR(64) columns.
pub fn hex_sha256(digest: [u8; 32]) -> String {
    digest.iter().fold(String::with_capacity(64), |mut acc, b| {
        acc.push_str(&format!("{:02x}", b));
        acc
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use pegana_common_verify::{AssetClass, PegState};
    use rust_decimal::Decimal;
    use std::collections::HashMap;
    use std::str::FromStr;

    fn sample_inputs() -> InputsFrozen {
        InputsFrozen {
            asset: "USDC".into(),
            class: AssetClass::StableFiat,
            now: Utc::now(),
            alpha: Decimal::from_str("0.3").unwrap(),
            intrinsic_usd: Decimal::ONE,
            market_usd: Decimal::from_str("0.9999").unwrap(),
            intrinsic_sol: None,
            market_sol: None,
            ewma_prev: None,
            hyusd_cr: None,
            previous_state: PegState::Pegged,
            candidate_state: None,
            candidate_since: None,
            thresholds: HashMap::new(),
            threshold_kind: "bps".into(),
            pyth_entries: HashMap::new(),
            confirm_up_secs: 30,
            decay_down_secs: 120,
        }
    }

    fn sample_computed() -> Computed {
        Computed {
            discount_raw: Decimal::from_str("0.0001").unwrap(),
            discount_smooth: Decimal::from_str("0.0001").unwrap(),
            final_state: PegState::Pegged,
            confidence_label: "high".into(),
        }
    }

    #[test]
    fn assets_hash_is_stable_across_comment_changes() {
        let with_comment = "[[assets]]\nsymbol = \"USDC\"\n# important comment\n";
        let without = "[[assets]]\nsymbol = \"USDC\"\n";
        assert_eq!(
            canonical_assets_hash(with_comment).unwrap(),
            canonical_assets_hash(without).unwrap()
        );
    }

    #[test]
    fn assets_hash_changes_when_value_changes() {
        let v1 = "[[assets]]\nsymbol = \"USDC\"\n";
        let v2 = "[[assets]]\nsymbol = \"USDT\"\n";
        assert_ne!(
            canonical_assets_hash(v1).unwrap(),
            canonical_assets_hash(v2).unwrap()
        );
    }

    #[test]
    fn receipt_hash_is_deterministic() {
        let inputs = sample_inputs();
        let computed = sample_computed();
        let h1 = canonical_receipt_hash(
            "0.1.0",
            None,
            "[[assets]]\nsymbol=\"USDC\"\n",
            &inputs,
            &computed,
        )
        .unwrap();
        let h2 = canonical_receipt_hash(
            "0.1.0",
            None,
            "[[assets]]\nsymbol=\"USDC\"\n",
            &inputs,
            &computed,
        )
        .unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn receipt_hash_changes_when_version_changes() {
        let inputs = sample_inputs();
        let computed = sample_computed();
        let h1 = canonical_receipt_hash("0.1.0", None, "x", &inputs, &computed).unwrap();
        let h2 = canonical_receipt_hash("0.2.0", None, "x", &inputs, &computed).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn hex_encoding_is_64_chars() {
        let digest = [0u8; 32];
        assert_eq!(hex_sha256(digest).len(), 64);
    }
}
