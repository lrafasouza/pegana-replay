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

#[tokio::main]
async fn main() -> Result<()> {
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
            "Install the matching version:\n  cargo install pegana-replay-cli --version {}",
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

    let mut memo_payloads: Vec<String> = Vec::new();
    if let EncodedTransaction::Json(ui_tx) = &tx.transaction.transaction {
        if let UiMessage::Parsed(parsed) = &ui_tx.message {
            for ix in &parsed.instructions {
                collect_memo(ix, &mut memo_payloads);
            }
        }
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
    //    Find a memo whose payload contains the receipt's expected_receipt_sha256.
    let expected_sha = &receipt.expected_receipt_sha256;
    let matched = memo_payloads.iter().any(|payload| {
        payload.starts_with("pegana-v1|") && payload.contains(expected_sha.as_str())
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
        println!("      explorer: https://solscan.io/tx/{}", tx_sig_str);
    }
    Ok(())
}
