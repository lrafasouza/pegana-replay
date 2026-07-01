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
        state_reason: None,
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

// ── PASS output wording: v1 vs v2 ───────────────────────────────────────────

/// v1 PASS line must advertise that it is tamper-evident only (no re-derivation).
#[test]
fn v1_pass_line_says_tamper_evident_only() {
    // synth_receipt() produces a v1 receipt.
    let receipt = synth_receipt();
    assert_eq!(receipt.schema_version, "v1");
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
    assert!(
        stdout.contains("tamper-evident only"),
        "v1 PASS line must contain 'tamper-evident only'; stdout: {stdout}"
    );
    assert!(
        !stdout.contains("(re-derived)"),
        "v1 PASS line must NOT contain '(re-derived)'; stdout: {stdout}"
    );
}

/// v2 PASS line must still say "(re-derived)".
#[test]
#[cfg(feature = "workspace-tests")]
fn v2_pass_line_says_re_derived() {
    // Use the committed USDe-004 v2 bundle (the hash-gate bundle from the spec).
    let bundle = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/backtests/2025-10-10-usde-depeg/receipts/USDe-004.json");
    let output = Command::new(env!("CARGO_BIN_EXE_pegana-replay"))
        .arg("--bundle")
        .arg(&bundle)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("(re-derived)"),
        "v2 PASS line must contain '(re-derived)'; stdout: {stdout}"
    );
    assert!(
        !stdout.contains("tamper-evident only"),
        "v2 PASS line must NOT contain 'tamper-evident only'; stdout: {stdout}"
    );
}

// ── Fix 2: ERROR paths must exit 2, not 1 ───────────────────────────────────

#[test]
fn error_exit2_on_malformed_json_bundle() {
    // Write a file with invalid JSON — parse fails → ERROR → exit 2.
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(b"{ this is not valid json }").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_pegana-replay"))
        .arg("--bundle")
        .arg(f.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit 2 (ERROR); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ERROR"),
        "expected ERROR prefix on stderr; got: {stderr}"
    );
}

#[test]
fn error_exit2_on_nonexistent_bundle_path() {
    // Point to a path that definitely does not exist → read fails → ERROR → exit 2.
    let output = Command::new(env!("CARGO_BIN_EXE_pegana-replay"))
        .arg("--bundle")
        .arg("/tmp/pegana_replay_test_nonexistent_bundle_abc123.json")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit 2 (ERROR); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ERROR"),
        "expected ERROR prefix on stderr; got: {stderr}"
    );
}

// ── Fix 2 + existing version logic: VERSION_MISMATCH exits 3 ────────────────

#[test]
fn version_mismatch_exit3_on_bogus_version() {
    // Synthesise a valid receipt, then swap the methodology_version to a
    // string that will never match the embedded CLI version.
    let mut receipt = synth_receipt();
    receipt.methodology_version = "0.0.0-bogus-test-version".into();
    // The sha256 no longer matches either, but the version check fires first
    // (exit 3) before the hash check (exit 1) so the exit code is 3.
    let file = write_bundle(&receipt);
    let output = Command::new(env!("CARGO_BIN_EXE_pegana-replay"))
        .arg("--bundle")
        .arg(file.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        output.status.code(),
        Some(3),
        "expected exit 3 (VERSION_MISMATCH); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("VERSION_MISMATCH"),
        "expected VERSION_MISMATCH on stderr; got: {stderr}"
    );
}
