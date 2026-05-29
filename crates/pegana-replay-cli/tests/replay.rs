//! Integration tests for `pegana-replay`.
//!
//! Synthesize a Receipt via the methodology crate (the same code path
//! the engine uses in prod), serialize to a temp JSON file, invoke the
//! just-built binary via `CARGO_BIN_EXE_pegana-replay`, assert PASS
//! exit-0 stdout AND that tampering with `market_usd` flips it to
//! exit-1 with "FAIL" on stderr. Covers AC30, AC31, AC32, AC34.

use chrono::Utc;
use pegana_common_verify::{AssetClass, PegState};
use pegana_methodology::{
    canonical_receipt_hash, hex_sha256, methodology_git_sha, methodology_version, Computed,
    InputsFrozen, Receipt,
};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::io::Write;
use std::process::Command;
use std::str::FromStr;

fn synth_receipt() -> Receipt {
    let assets_toml = "[[assets]]\nsymbol = \"USDC\"\n";
    let inputs = InputsFrozen {
        asset: "USDC".into(),
        class: AssetClass::StableFiat,
        now: Utc::now(),
        alpha: Decimal::from_str("0.3").unwrap(),
        intrinsic_usd: Decimal::ONE,
        market_usd: Decimal::from_str("0.99").unwrap(),
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
    };
    let computed = Computed {
        discount_raw: Decimal::from_str("0.01").unwrap(),
        discount_smooth: Decimal::from_str("0.01").unwrap(),
        final_state: PegState::Drift,
        confidence_label: "high".into(),
    };
    let canonical = toml::to_string(&toml::from_str::<toml::Value>(assets_toml).unwrap()).unwrap();
    let hash = canonical_receipt_hash(
        methodology_version(),
        methodology_git_sha(),
        &canonical,
        &inputs,
        &computed,
    )
    .unwrap();
    Receipt {
        schema_version: "v1".into(),
        methodology_version: methodology_version().to_string(),
        methodology_git_sha: methodology_git_sha().map(str::to_string),
        assets_toml_canonical: canonical,
        inputs_frozen: inputs,
        expected_computed: computed,
        expected_receipt_sha256: hex_sha256(hash),
    }
}

fn write_bundle(receipt: &Receipt) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(serde_json::to_string(receipt).unwrap().as_bytes())
        .unwrap();
    f
}

#[test]
fn pass_on_clean_bundle() {
    let receipt = synth_receipt();
    let file = write_bundle(&receipt);
    let output = Command::new(env!("CARGO_BIN_EXE_pegana-replay"))
        .arg("--bundle")
        .arg(file.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("PASS"), "stdout: {stdout}");
}

#[test]
fn fail_when_input_tampered() {
    let mut receipt = synth_receipt();
    receipt.inputs_frozen.market_usd = Decimal::from_str("0.50").unwrap();
    // expected_receipt_sha256 untouched → recompute will diverge.
    let file = write_bundle(&receipt);
    let output = Command::new(env!("CARGO_BIN_EXE_pegana-replay"))
        .arg("--bundle")
        .arg(file.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("FAIL"), "stderr: {stderr}");
}
