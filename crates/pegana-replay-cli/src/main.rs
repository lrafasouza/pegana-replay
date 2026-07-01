//! pegana-replay — verify a Pegana alert against its receipt.
//!
//! Trust Layer's verifier-of-record. Fetches a Receipt from the API
//! (`/v1/audit/:id/replay-bundle`) or a local `--bundle` JSON, then
//! re-hashes the receipt's frozen inputs + recorded verdict and compares
//! to the stored hash. Verifies the canonical receipt hash and its
//! signer-pinned on-chain anchor, so anyone can confirm the published
//! history wasn't altered (tamper-evidence). The CLI does NOT re-execute
//! the methodology or re-derive the verdict — see ADR-0019.
//!
//! Exit codes (per `docs/pegana-trust-layer-v0.1.0/10-distribution.md` §8):
//!   0 — PASS  (hash matches)
//!   1 — FAIL  (receipt sha256 mismatch — tamper or corruption)
//!   2 — ERROR (network failure, malformed bundle, unknown alert)
//!   3 — VERSION_MISMATCH (install the matching CLI version)
//!   4 — ONCHAIN_MISMATCH (only when `--verify-onchain`)

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use pegana_methodology::{
    canonical_assets_hash, canonical_receipt_hash, hex_sha256, methodology_version, rederive,
    Receipt,
};
use std::path::PathBuf;
use uuid::Uuid;

/// The set of accepted Pegana ops/commit wallet addresses.
///
/// This allowlist is what makes the on-chain check trustless: the verifier
/// independently pins the accepted signers at compile time.  Without this pin,
/// anyone could post an identical memo from any wallet — since the receipt sha256
/// is public — and the verifier would accept it.  Pinning the signers here means
/// only an actual Pegana commit wallet produces a valid on-chain attestation,
/// regardless of what tx_sig the API returns.
///
/// # Rotation model
///
/// When the ops wallet rotates, ADD the new key to this array — keep the old key
/// in place.  Both old and new remain valid forever so that historical receipts
/// signed by the previous wallet continue to verify correctly.  This MUST remain
/// a compile-time pin (no runtime env-var or flag override); removing that
/// guarantee would allow an attacker to inject an arbitrary signer at runtime and
/// break the trustless property.  A rotation is shipped as a source edit that
/// produces a new CLI release; users who build from source get the update by
/// pulling and rebuilding.
const PEGANA_COMMIT_SIGNERS: &[&str] = &["7PpoyumFQMmcWzhJxDYr6iPv1fjYN41KBTA8xKKzu7R9"];

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Alert UUID to verify by fetching from the API.
    #[arg(long, conflicts_with = "bundle")]
    alert_id: Option<Uuid>,

    /// Path to a local replay bundle JSON.
    #[arg(long, conflicts_with = "alert_id")]
    bundle: Option<PathBuf>,

    /// API base URL.
    #[arg(long, env = "PEGANA_API", default_value = "https://api.pegana.xyz")]
    api_url: String,

    /// Additionally verify the on-chain memo commitment.
    #[arg(long)]
    verify_onchain: bool,

    /// Solana RPC URL for --verify-onchain.
    #[arg(
        long,
        env = "SOLANA_RPC_URL",
        default_value = "https://api.mainnet-beta.solana.com"
    )]
    solana_rpc: String,

    /// Suppress all output except final PASS/FAIL.
    #[arg(long)]
    quiet: bool,
}

/// Only `not_applicable` (DRIFT / cost-gated alerts that are never anchored)
/// may be skipped silently. Every other non-committed status means an anchor
/// is expected and its absence must surface as a non-pass.
fn onchain_skip_is_ok(commit_status: &str) -> bool {
    commit_status == "not_applicable"
}

/// Check that the fee-payer / first account key of the transaction is in the
/// compile-time signer allowlist.
///
/// `pubkey`    — the `pubkey` field of `account_keys[0]` in the parsed tx.
/// `is_signer` — the `signer` field of the same entry (must be true for the
///               fee-payer; defensive check).
/// `allowed`   — the slice of accepted wallet addresses (PEGANA_COMMIT_SIGNERS).
///
/// Returns `true` only when BOTH conditions hold: the address is in the
/// allowlist AND the entry is flagged as a signer.  This is a pure function
/// with no I/O so it can be unit-tested without a real RPC call.
fn onchain_signer_ok(pubkey: &str, is_signer: bool, allowed: &[&str]) -> bool {
    is_signer && allowed.contains(&pubkey)
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("ERROR  {e:#}");
        std::process::exit(2);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    let receipt: Receipt = match (&cli.alert_id, &cli.bundle) {
        (Some(id), _) => fetch_bundle(&cli.api_url, *id).await?,
        (None, Some(path)) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read bundle {}", path.display()))?;
            serde_json::from_str(&text).context("parse bundle JSON")?
        }
        (None, None) => bail!("provide either --alert-id <UUID> or --bundle <path>"),
    };

    verify(&receipt, cli.quiet)?;

    if cli.verify_onchain {
        if let Some(id) = cli.alert_id {
            verify_onchain(&cli.api_url, &cli.solana_rpc, id, &receipt, cli.quiet).await?;
        } else if !cli.quiet {
            // For --bundle only: skip onchain check (no alert_id known).
            eprintln!("Note: --verify-onchain requires --alert-id (skipped for --bundle)");
        }
    }

    Ok(())
}

async fn fetch_bundle(api_url: &str, alert_id: Uuid) -> Result<Receipt> {
    let url = format!(
        "{}/v1/audit/{}/replay-bundle",
        api_url.trim_end_matches('/'),
        alert_id
    );
    let resp = reqwest::get(&url)
        .await
        .with_context(|| format!("GET {}", url))?;
    if !resp.status().is_success() {
        bail!("API returned {}: {}", resp.status(), url);
    }
    let receipt: Receipt = resp.json().await.context("decode bundle JSON")?;
    Ok(receipt)
}

fn verify(receipt: &Receipt, quiet: bool) -> Result<()> {
    // 1) Schema version — accept v1 (re-hash only) and v2 (re-hash + re-derive).
    if !matches!(receipt.schema_version.as_str(), "v1" | "v2") {
        bail!("unsupported schema_version: {}", receipt.schema_version);
    }

    // 2) Methodology version match
    let cli_version = methodology_version();
    if receipt.methodology_version != cli_version {
        eprintln!(
            "VERSION_MISMATCH: alert uses methodology v{}; this CLI is v{}.",
            receipt.methodology_version, cli_version
        );
        eprintln!(
            "Install the matching version:\n  curl --proto '=https' --tlsv1.2 -LsSf \
             https://releases.pegana.xyz/install.sh | PEGANA_REPLAY_VERSION=v{} sh",
            receipt.methodology_version
        );
        std::process::exit(3);
    }

    // 3) Verify canonical hashes — version-agnostic tamper-evidence (v1 and v2).
    let actual_assets_hash = canonical_assets_hash(&receipt.assets_toml_canonical)
        .map_err(|e| anyhow!("canonical_assets_hash failed: {e}"))?;
    let actual_assets_hex = hex_sha256(actual_assets_hash);

    let recomputed_hash = canonical_receipt_hash(
        &receipt.methodology_version,
        receipt.methodology_git_sha.as_deref(),
        &receipt.assets_toml_canonical,
        &receipt.inputs_frozen,
        &receipt.expected_computed,
    )
    .map_err(|e| anyhow!("canonical_receipt_hash failed: {e}"))?;
    let recomputed_hex = hex_sha256(recomputed_hash);

    if recomputed_hex != receipt.expected_receipt_sha256 {
        eprintln!("FAIL  receipt sha256 mismatch");
        eprintln!("  expected: {}", receipt.expected_receipt_sha256);
        eprintln!("  computed: {}", recomputed_hex);
        std::process::exit(1);
    }

    // 4) Re-derivation check (v2 only). Re-hash passes → tamper-evidence holds.
    //    Re-derivation additionally proves the frozen inputs actually produce the
    //    recorded verdict — the Grant Thesis ("anyone can recompute our peg history").
    //
    //    v1 receipts are skipped: capture-ordering bugs (GAP-1/2/3) mean their
    //    frozen ewma_prev / candidate_* / previous_state values are incorrect
    //    post-mutation snapshots; re-derivation would produce spurious mismatches.
    let rederived_note = if receipt.schema_version == "v2" {
        let computed = &receipt.expected_computed;
        let r = rederive(&receipt.inputs_frozen)
            .map_err(|e| anyhow!("re-derivation error (frozen inputs inconsistent): {e}"))?;
        if r.discount_raw != computed.discount_raw
            || r.discount_smooth != computed.discount_smooth
            || r.final_state != computed.final_state
        {
            eprintln!("FAIL  re-derivation mismatch");
            eprintln!(
                "  expected: discount_raw={} discount_smooth={} final_state={:?}",
                computed.discount_raw, computed.discount_smooth, computed.final_state
            );
            eprintln!(
                "  derived:  discount_raw={} discount_smooth={} final_state={:?}",
                r.discount_raw, r.discount_smooth, r.final_state
            );
            std::process::exit(1);
        }
        " (re-derived)"
    } else {
        " (tamper-evident only; v1 not re-derived)"
    };

    if !quiet {
        let inputs = &receipt.inputs_frozen;
        let computed = &receipt.expected_computed;
        println!(
            "PASS  {}  {:?} -> {:?}  @ {}{}",
            inputs.asset, inputs.previous_state, computed.final_state, inputs.now, rederived_note
        );
        println!(
            "      methodology v{}, assets_toml sha256:{}",
            receipt.methodology_version,
            &actual_assets_hex[..12],
        );
        println!(
            "      recomputed receipt:{} matches stored receipt:{}",
            &recomputed_hex[..12],
            &receipt.expected_receipt_sha256[..12],
        );
    }
    Ok(())
}

async fn verify_onchain(
    api_url: &str,
    solana_rpc: &str,
    alert_id: Uuid,
    receipt: &Receipt,
    quiet: bool,
) -> Result<()> {
    // 1) Fetch tx_sig from API.
    let url = format!(
        "{}/v1/audit/{}/onchain",
        api_url.trim_end_matches('/'),
        alert_id
    );
    let resp = reqwest::get(&url)
        .await
        .with_context(|| format!("GET {}", url))?;
    if resp.status() == 404 {
        // Parse commit_status to decide whether silence is safe.
        let body_404: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        let status = body_404["commit_status"].as_str().unwrap_or("unknown");
        if onchain_skip_is_ok(status) {
            if !quiet {
                eprintln!("Note: no on-chain anchor for this alert (commit_status: {status}) — expected for cost-gated transitions.");
            }
            return Ok(());
        }
        eprintln!("ANCHOR NOT VERIFIED (status: {status})");
        std::process::exit(4);
    }
    if !resp.status().is_success() {
        bail!("API returned {}", resp.status());
    }
    let body: serde_json::Value = resp.json().await?;
    let tx_sig_str = body["tx_sig"].as_str().context("tx_sig missing")?;

    // 2) Parse the signature + connect to Solana RPC.
    use solana_client::nonblocking::rpc_client::RpcClient;
    use solana_sdk::commitment_config::CommitmentConfig;
    use solana_sdk::signature::Signature;
    use solana_transaction_status::{
        EncodedTransaction, UiInstruction, UiMessage, UiTransactionEncoding,
    };
    use std::str::FromStr;

    let sig = Signature::from_str(tx_sig_str)
        .with_context(|| format!("invalid Solana signature returned by API: {tx_sig_str}"))?;
    let rpc = RpcClient::new_with_commitment(solana_rpc.to_string(), CommitmentConfig::confirmed());

    // 3) Fetch the transaction. JsonParsed gives us decoded memo instruction data
    //    without us having to manually base58 decode + slice the program log line.
    let tx = rpc
        .get_transaction(&sig, UiTransactionEncoding::JsonParsed)
        .await
        .with_context(|| format!("RPC get_transaction({tx_sig_str}) failed"))?;

    // 4) Extract memo payload(s) from instructions. SPL Memo program embeds the
    //    UTF-8 payload as the instruction's `data` field when encoded as
    //    JsonParsed (the helper decodes it for us).
    //
    // Wave 13C audit C P1: we now ALSO walk `meta.inner_instructions` so a
    // future engine version that wraps the memo in a CPI / bundle still
    // produces a verifiable receipt — the engine currently emits single-IX
    // memo txs, but the wrapper change would otherwise turn into a silent
    // ONCHAIN_MISMATCH exit 4 with no traceable root cause.
    const MEMO_PROGRAM: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

    // Collect memo payloads from one UiInstruction (parsed branch). The
    // `serde_json::to_string` -> reparse pattern is the cheapest way to
    // reach the UntaggedJSON shape Solana ships under `parsed`, which is
    // the actual decoded memo string for the SPL Memo program.
    fn collect_memo(ix: &UiInstruction, sink: &mut Vec<String>) {
        let json_str = serde_json::to_string(ix).unwrap_or_default();
        let v: serde_json::Value =
            serde_json::from_str(&json_str).unwrap_or(serde_json::Value::Null);
        let program_id = v["programId"].as_str().unwrap_or("");
        if program_id != MEMO_PROGRAM {
            return;
        }
        if let Some(s) = v["parsed"].as_str() {
            sink.push(s.to_string());
        }
    }

    // 4b) HARD GATE: we can only verify the signer from a Json + Parsed message.
    //     Any other encoding means we cannot determine the signer → fail safe.
    //
    //     SECURITY INVARIANT: no memo (top-level OR inner-instruction) is
    //     collected unless the signer was verified first.  The inner-instruction
    //     scan must live INSIDE this gate, not after it, because `meta` is a
    //     sibling field present for ANY encoding — an attacker-controlled RPC
    //     could return a non-Json encoding (Binary/LegacyBinary/Accounts) so the
    //     `if let Json` does not match and the signer check is skipped, while the
    //     inner scan would still collect a forged memo and pass exit 0.
    let parsed = match &tx.transaction.transaction {
        EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
            UiMessage::Parsed(parsed) => parsed,
            _ => {
                // Non-Parsed message (Raw/Legacy): cannot determine fee-payer identity.
                // Fail safe: treating an unverifiable signer as a mismatch is
                // the only trustless interpretation.
                eprintln!(
                    "FAIL  on-chain tx {} returned a non-Parsed message encoding — \
                     cannot verify the commit signer (fail-safe exit).",
                    tx_sig_str
                );
                eprintln!("Solscan: https://solscan.io/tx/{}", tx_sig_str);
                std::process::exit(4);
            }
        },
        _ => {
            // Non-Json encoding (Binary/LegacyBinary/Accounts): cannot determine signer.
            eprintln!(
                "FAIL  on-chain tx {} returned a non-Json encoding — \
                 cannot verify the commit signer (fail-safe exit).",
                tx_sig_str
            );
            eprintln!("Solscan: https://solscan.io/tx/{}", tx_sig_str);
            std::process::exit(4);
        }
    };

    // Verify the fee-payer (account_keys[0]) is in the PEGANA_COMMIT_SIGNERS allowlist.
    let fee_payer_pubkey = match parsed.account_keys.first() {
        None => {
            eprintln!(
                "FAIL  on-chain tx {} has no account_keys — cannot verify signer",
                tx_sig_str
            );
            eprintln!("Solscan: https://solscan.io/tx/{}", tx_sig_str);
            std::process::exit(4);
        }
        Some(fee_payer) => {
            if !onchain_signer_ok(&fee_payer.pubkey, fee_payer.signer, PEGANA_COMMIT_SIGNERS) {
                eprintln!(
                    "FAIL  on-chain tx {} was NOT signed by an accepted Pegana commit wallet.",
                    tx_sig_str
                );
                eprintln!("  observed signer : {}", fee_payer.pubkey);
                eprintln!("  accepted signers : {:?}", PEGANA_COMMIT_SIGNERS);
                eprintln!("Solscan: https://solscan.io/tx/{}", tx_sig_str);
                std::process::exit(4);
            }
            fee_payer.pubkey.clone()
        }
    };

    // ONLY NOW — signer verified — collect memos (top-level AND inner-instructions).
    let mut memo_payloads: Vec<String> = Vec::new();
    for ix in &parsed.instructions {
        collect_memo(ix, &mut memo_payloads);
    }
    // Also scan inner instructions (CPI calls). `meta.inner_instructions` is
    // an OptionSerializer<Vec<UiInnerInstructions>> wrapper; flatten via
    // Option::from so we drop both `None` and `Skip` cleanly.
    if let Some(meta) = tx.transaction.meta.as_ref() {
        if let Some(inner_groups) = Option::<Vec<_>>::from(meta.inner_instructions.clone()) {
            for group in inner_groups {
                for ix in &group.instructions {
                    collect_memo(ix, &mut memo_payloads);
                }
            }
        }
    }

    if memo_payloads.is_empty() {
        eprintln!(
            "FAIL  on-chain tx {} has no SPL Memo instruction — cannot verify receipt sha256",
            tx_sig_str
        );
        std::process::exit(4);
    }

    // 5) Memo format from engine onchain_commit.rs: "pegana-v1|<version>|<alert_id>|<receipt_sha256>"
    //    Parse the `|`-delimited fields and require:
    //      field[0] == "pegana-v1"    (scheme guard)
    //      field[last] == expected_sha  (exact match, no substring false-positive)
    //    Middle fields (<version>, <alert_id>) are accepted leniently.
    let expected_sha = &receipt.expected_receipt_sha256;
    let matched = memo_payloads.iter().any(|payload| {
        let parts: Vec<&str> = payload.splitn(4, '|').collect();
        parts.first().copied() == Some("pegana-v1")
            && parts.last().copied() == Some(expected_sha.as_str())
    });

    if !matched {
        eprintln!(
            "FAIL  on-chain memo at {} does NOT match receipt sha256:",
            tx_sig_str
        );
        eprintln!("  expected sha256 in memo: {}", expected_sha);
        eprintln!("  memo payloads observed:");
        for p in &memo_payloads {
            eprintln!("    - {}", p);
        }
        eprintln!("Solscan: https://solscan.io/tx/{}", tx_sig_str);
        std::process::exit(4);
    }

    if !quiet {
        println!(
            "      on-chain memo verified: tx {} carries pegana-v1|...|{}",
            tx_sig_str,
            &expected_sha[..12]
        );
        println!("      on-chain signer confirmed: {}", fee_payer_pubkey);
        println!("      explorer: https://solscan.io/tx/{}", tx_sig_str);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{onchain_signer_ok, onchain_skip_is_ok, verify};
    use chrono::DateTime;
    use pegana_common_verify::{AssetClass, PegState};
    use pegana_methodology::{
        canonical_assets_hash, canonical_receipt_hash, hex_sha256, methodology_version,
        receipt::{Computed, InputsFrozen, PythEntry},
        Receipt,
    };
    use rust_decimal::Decimal;
    use std::collections::HashMap;
    use std::str::FromStr;

    // ── helper: build a syntactically-valid receipt with a correct hash ──────

    fn stub_inputs(previous_state: PegState) -> InputsFrozen {
        InputsFrozen {
            asset: "USDC".into(),
            class: AssetClass::StableFiat,
            now: DateTime::from_timestamp(1_704_067_200, 0).unwrap(),
            alpha: Decimal::from_str("0.3").unwrap(),
            intrinsic_usd: Decimal::ONE,
            market_usd: Decimal::from_str("0.999").unwrap(), // 10bps discount, under drift(20)
            intrinsic_sol: None,
            market_sol: None,
            ewma_prev: None,
            hyusd_cr: None,
            previous_state,
            candidate_state: None,
            candidate_since: None,
            thresholds: {
                let mut m = HashMap::new();
                m.insert("drift".into(), 20u32);
                m.insert("depeg".into(), 100u32);
                m.insert("critical".into(), 300u32);
                m
            },
            threshold_kind: "bps".into(),
            pyth_entries: HashMap::<String, PythEntry>::new(),
            confirm_up_secs: 30,
            decay_down_secs: 120,
        }
    }

    fn stub_computed() -> Computed {
        Computed {
            // discount = 1 - 0.999/1.0 = 0.001 = 10bps, under drift(20) → Pegged
            discount_raw: Decimal::from_str("0.001").unwrap(),
            discount_smooth: Decimal::from_str("0.001").unwrap(), // cold-start seeds at raw
            final_state: PegState::Pegged,
            confidence_label: "high".into(),
        }
    }

    const STUB_TOML: &str = "[[assets]]\nsymbol = \"USDC\"\n";

    fn make_receipt(schema_version: &str, inputs: InputsFrozen, computed: Computed) -> Receipt {
        let method_ver = methodology_version();
        // assets_hash is recomputed internally by verify(); we don't store it in
        // the Receipt struct. Computing it here only to validate STUB_TOML is parseable.
        let _assets_hash = canonical_assets_hash(STUB_TOML)
            .map(hex_sha256)
            .expect("canonical_assets_hash failed on STUB_TOML");
        let receipt_hash = canonical_receipt_hash(method_ver, None, STUB_TOML, &inputs, &computed)
            .map(hex_sha256)
            .expect("canonical_receipt_hash failed");
        Receipt {
            schema_version: schema_version.into(),
            methodology_version: method_ver.to_string(),
            methodology_git_sha: None,
            assets_toml_canonical: STUB_TOML.into(),
            inputs_frozen: inputs,
            expected_computed: computed,
            expected_receipt_sha256: receipt_hash,
            state_reason: None,
        }
    }

    // ── v2 receipt: happy-path round-trip ────────────────────────────────────

    /// A v2 receipt with correct inputs and hash must verify (PASS).
    #[test]
    fn v2_receipt_verify_pass() {
        let receipt = make_receipt("v2", stub_inputs(PegState::Pegged), stub_computed());
        // verify() calls std::process::exit on failure, so this returning Ok is
        // proof the path succeeded.
        assert!(verify(&receipt, true).is_ok());
    }

    /// A v1 receipt passes verify (re-hash only, no re-derive).
    #[test]
    fn v1_receipt_verify_pass_no_rederive() {
        let receipt = make_receipt("v1", stub_inputs(PegState::Pegged), stub_computed());
        assert!(verify(&receipt, true).is_ok());
    }

    /// An unknown schema_version returns an error (not process::exit).
    #[test]
    fn unknown_schema_version_returns_err() {
        let receipt = make_receipt("v3", stub_inputs(PegState::Pegged), stub_computed());
        let result = verify(&receipt, true);
        assert!(result.is_err(), "unknown schema_version must error");
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("unsupported schema_version"),
            "wrong error: {}",
            msg
        );
    }

    /// A v2 receipt where expected_computed disagrees with what rederive produces
    /// — but the hash was computed over the dishonest computed — causes exit(1).
    ///
    /// This is the real attack re-derivation defends against: someone who
    /// controls the server could fabricate a receipt where the stated final_state
    /// is "PEGGED" but the inputs actually derive to "DRIFT". The hash covers
    /// the (inputs, computed) pair consistently, so re-hash alone passes. Only
    /// re-derivation catches this.
    ///
    /// We test this by passing a computed whose final_state = Pegged but whose
    /// inputs (11bps discount, no prior) actually rederive to Pegged too — we
    /// need to find an input combination where the hash-consistent pair passes
    /// re-hash but fails re-derive. The approach: build inputs with a 30bps
    /// discount (crosses drift=20 → rederive → Pegged because timer started,
    /// new candidate), and a matching honest computed (final_state=Pegged).
    /// Then modify the computed's final_state to Drift while keeping the receipt
    /// hash consistent with that MODIFIED pair — this makes re-hash pass but
    /// re-derive fail (rederive returns Pegged, stored says Drift).
    ///
    /// NOTE: because process::exit can't be caught in a unit test, we cannot
    /// call `verify()` directly for the exit-1 path. Instead we isolate the
    /// re-derive logic separately, which is the correct approach.
    #[test]
    fn v2_receipt_rederive_mismatch_is_detectable() {
        use pegana_methodology::rederive;
        // inputs: 10bps discount, cold-start, no EWMA prior → rederive produces
        // discount_raw=0.001, smooth=0.001, final_state=Pegged.
        let inputs = stub_inputs(PegState::Pegged);
        let honest_computed = stub_computed(); // final_state=Pegged (matches rederive)

        // A dishonest computed: claims Drift even though inputs rederive to Pegged.
        let dishonest_computed = Computed {
            discount_raw: honest_computed.discount_raw,
            discount_smooth: honest_computed.discount_smooth,
            final_state: PegState::Drift, // WRONG — inputs rederive to Pegged
            confidence_label: "high".into(),
        };

        // Build a hash-consistent pair over the DISHONEST computed (so re-hash passes).
        let method_ver = methodology_version();
        let dishonest_hash =
            canonical_receipt_hash(method_ver, None, STUB_TOML, &inputs, &dishonest_computed)
                .map(hex_sha256)
                .expect("hash");

        let dishonest_receipt = Receipt {
            schema_version: "v2".into(),
            methodology_version: method_ver.to_string(),
            methodology_git_sha: None,
            assets_toml_canonical: STUB_TOML.into(),
            inputs_frozen: inputs.clone(),
            expected_computed: dishonest_computed.clone(),
            expected_receipt_sha256: dishonest_hash,
            state_reason: None,
        };

        // Re-hash of dishonest_receipt would PASS (hash is consistent with (inputs, dishonest_computed)).
        // Re-derivation MUST produce Pegged (not Drift) — proving the mismatch is detectable.
        let rederived = rederive(&dishonest_receipt.inputs_frozen).expect("rederive");
        assert_eq!(
            rederived.final_state,
            PegState::Pegged,
            "rederive must return Pegged (inputs are 10bps → no drift)"
        );
        assert_ne!(
            rederived.final_state, dishonest_receipt.expected_computed.final_state,
            "dishonest computed claims Drift but rederive says Pegged — mismatch is caught"
        );

        // For completeness: the re-hash check alone passes on the dishonest receipt.
        // This confirms re-derivation is the ONLY check that catches this attack.
        let actual_hash = canonical_receipt_hash(
            method_ver,
            None,
            STUB_TOML,
            &dishonest_receipt.inputs_frozen,
            &dishonest_receipt.expected_computed,
        )
        .map(hex_sha256)
        .expect("hash");
        assert_eq!(
            actual_hash, dishonest_receipt.expected_receipt_sha256,
            "re-hash passes on dishonest receipt — only rederive catches the attack"
        );
    }

    const SIGNER: &str = "7PpoyumFQMmcWzhJxDYr6iPv1fjYN41KBTA8xKKzu7R9";
    const OTHER_SIGNER: &str = "So11111111111111111111111111111111111111112";
    // Simulates a future allowlist with two keys (old + rotated-in).
    const TWO_SIGNERS: &[&str] = &[SIGNER, OTHER_SIGNER];

    #[test]
    fn onchain_signer_ok_key_in_allowlist_returns_true() {
        // Primary signer present in a single-entry allowlist.
        assert!(onchain_signer_ok(SIGNER, true, &[SIGNER]));
    }

    #[test]
    fn onchain_signer_ok_key_not_in_allowlist_returns_false() {
        // A key that is NOT in the allowlist must be rejected even if is_signer=true.
        let intruder = "11111111111111111111111111111111";
        assert!(!onchain_signer_ok(intruder, true, &[SIGNER]));
    }

    #[test]
    fn onchain_signer_ok_signer_flag_false_returns_false() {
        // Even if the pubkey is in the allowlist, signer=false must fail.
        assert!(!onchain_signer_ok(SIGNER, false, &[SIGNER]));
    }

    #[test]
    fn onchain_signer_ok_second_key_in_two_entry_allowlist_returns_true() {
        // After a rotation the NEW key must also pass against the extended allowlist.
        assert!(onchain_signer_ok(OTHER_SIGNER, true, TWO_SIGNERS));
    }

    #[test]
    fn onchain_signer_ok_old_key_in_two_entry_allowlist_still_true() {
        // The OLD key must still pass so historical receipts keep verifying.
        assert!(onchain_signer_ok(SIGNER, true, TWO_SIGNERS));
    }

    // ── memo match logic (mirrors the closure in verify_onchain) ─────────────

    fn memo_matches(payload: &str, expected_sha: &str) -> bool {
        let parts: Vec<&str> = payload.splitn(4, '|').collect();
        parts.first().copied() == Some("pegana-v1") && parts.last().copied() == Some(expected_sha)
    }

    const SHA: &str = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";

    #[test]
    fn memo_match_well_formed_passes() {
        let memo = format!("pegana-v1|0.4.0|550e8400-e29b-41d4-a716-446655440000|{SHA}");
        assert!(memo_matches(&memo, SHA));
    }

    #[test]
    fn memo_match_wrong_scheme_fails() {
        // Changed scheme prefix must be rejected.
        let memo = format!("pegana-v2|0.4.0|550e8400-e29b-41d4-a716-446655440000|{SHA}");
        assert!(!memo_matches(&memo, SHA));
    }

    #[test]
    fn memo_match_wrong_sha_fails() {
        let other_sha = "0000000000000000000000000000000000000000000000000000000000000000";
        let memo = format!("pegana-v1|0.4.0|550e8400-e29b-41d4-a716-446655440000|{SHA}");
        assert!(!memo_matches(&memo, other_sha));
    }

    #[test]
    fn memo_match_sha_as_substring_is_rejected() {
        // The old `contains()` check would accept a memo whose last field is
        // "prefix_<sha>_suffix" as long as <sha> appeared anywhere.  The new
        // exact-last-field check must reject this.
        let memo =
            format!("pegana-v1|0.4.0|550e8400-e29b-41d4-a716-446655440000|prefix_{SHA}_extra");
        assert!(!memo_matches(&memo, SHA));
    }

    #[test]
    fn memo_match_middle_fields_lenient() {
        // Different version / alert_id values should still pass.
        let memo = format!("pegana-v1|9.9.9|ffffffff-ffff-ffff-ffff-ffffffffffff|{SHA}");
        assert!(memo_matches(&memo, SHA));
    }

    // ── 404 commit_status gate ────────────────────────────────────────────────

    #[test]
    fn onchain_404_pending_is_not_a_pass() {
        assert!(onchain_skip_is_ok("not_applicable"));
        for s in [
            "pending",
            "retry_exhausted",
            "wallet_drained",
            "persistence_failed",
            "unknown",
        ] {
            assert!(!onchain_skip_is_ok(s), "status {s} must not pass silently");
        }
    }
}
