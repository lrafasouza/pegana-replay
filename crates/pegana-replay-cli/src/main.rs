//! pegana-replay — verify a Pegana alert against its receipt.
//!
//! Trust Layer's verifier-of-record. Fetches a Receipt from the API
//! (`/v1/audit/:id/replay-bundle`) or a local `--bundle` JSON, then
//! re-runs `canonical_receipt_hash` through the methodology crate and
//! compares against the stored hash. Designed to be auditable by a
//! skeptic with nothing but the binary and a published alert ID.
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
    canonical_assets_hash, canonical_receipt_hash, hex_sha256, methodology_version, Receipt,
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
    // 1) Schema version
    if receipt.schema_version != "v1" {
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

    // 3) Verify canonical hashes
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

    if !quiet {
        let inputs = &receipt.inputs_frozen;
        let computed = &receipt.expected_computed;
        println!(
            "PASS  {}  {:?} -> {:?}  @ {}",
            inputs.asset, inputs.previous_state, computed.final_state, inputs.now
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
        eprintln!("Warning: on-chain commit not yet completed for this alert.");
        return Ok(());
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
    use super::onchain_signer_ok;

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
}
