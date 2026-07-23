//! pegana-replay — verify a Pegana alert against its receipt.
//!
//! Trust Layer's verifier-of-record. Fetches a Receipt from the API
//! (`/v1/audit/:id/replay-bundle`) or a local `--bundle` JSON, then
//! re-hashes the receipt's frozen inputs + recorded verdict and compares
//! to the stored canonical hash — this alone proves the receipt's fields
//! are internally self-consistent with the hash the server returned, NOT
//! that the server didn't swap in a different, equally self-consistent
//! receipt after the fact.
//!
//! By DEFAULT (v0.5.0+), when run with `--alert-id`, the CLI additionally
//! fetches the on-chain SPL Memo commitment for the alert and confirms (a)
//! it was signed by one of Pegana's compile-time-pinned ops wallets and (b)
//! its payload carries this exact receipt hash. THIS is the check that
//! actually rules out post-hoc substitution: Solana's runtime only lands a
//! transaction whose declared signer accounts produced a valid ed25519
//! signature over it, so only the holder of the ops wallet's key can
//! produce a matching anchor. Pass `--offline` to skip this and rely on
//! the hash check alone (CI, air-gapped hosts). `--bundle` mode is always
//! offline — a local file carries no `alert_id` to look an anchor up with.
//!
//! For schema-v2 receipts the CLI additionally re-derives the verdict from
//! the frozen inputs. It does NOT re-execute the methodology against fresh
//! oracle data, and it does NOT independently re-verify the ed25519
//! signature bytes — it trusts the queried RPC's report that the account
//! was a signer (cross-check the printed Solscan link against another RPC
//! for a fully independent read). See ADR-0019 and ADR-0033.
//!
//! Exit codes (amends `docs/pegana-trust-layer-v0.1.0/10-distribution.md`
//! §8 per ADR-0033):
//!   0 — PASS  (hash matches; if --alert-id was used without --offline,
//!              the on-chain anchor + pinned signer also matched)
//!   1 — FAIL  (receipt sha256 mismatch, or v2 re-derivation mismatch —
//!              tamper or corruption)
//!   2 — ERROR (network failure fetching the bundle, malformed bundle,
//!              unknown alert, or bad CLI usage)
//!   3 — VERSION_MISMATCH (install the CLI build matching the receipt's
//!              methodology_version)
//!   4 — ONCHAIN_MISMATCH (a deliberate on-chain check FAILURE: wrong or
//!              absent signer, no memo instruction, memo content mismatch,
//!              or anchoring was expected and permanently failed —
//!              retry_exhausted / wallet_drained)
//!   5 — ONCHAIN_INCOMPLETE (the on-chain check could NOT be attempted or
//!              completed — RPC/API unreachable, the anchor is still inside
//!              its 24h commit window (ADR-0004 `pending`), OR a severe
//!              transition (Depeg/Critical/BlackSwan) carries a terminal
//!              `not_applicable` because on-chain commit was disabled for the
//!              emitting deployment (no PEGANA_COMMIT_KEYPAIR) — the anchor the
//!              policy expects is absent, not tampered. NOT a mismatch; the
//!              hash result above still stands. Only reachable when --alert-id
//!              is used without --offline.)

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use pegana_common_verify::PegState;
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

/// Verify a Pegana alert against its published receipt.
#[derive(Parser, Debug)]
#[command(
    version,
    about,
    long_about = "Verify a Pegana alert against its published receipt.\n\n\
        Always checks: the receipt's canonical SHA-256, recomputed from its \
        frozen inputs, matches the hash the API/bundle claims (proves \
        internal self-consistency).\n\n\
        By default, when --alert-id is used, ALSO checks: the same hash is \
        anchored on Solana in an SPL Memo signed by a pinned Pegana ops \
        wallet (proves the receipt was not swapped after the fact). Skip \
        this with --offline. --bundle mode is always offline — a local \
        file carries no alert_id to look an anchor up with."
)]
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

    /// Skip the on-chain anchor check and rely on the hash check alone.
    /// On-chain verification (SPL Memo + pinned signer) runs BY DEFAULT
    /// whenever --alert-id is used (v0.5.0+); pass --offline for CI,
    /// air-gapped hosts, or when you deliberately only want the fast
    /// hash-only tamper-evidence check. --bundle mode is always offline.
    #[arg(long, alias = "no-onchain")]
    offline: bool,

    /// Deprecated, kept for backward compatibility with existing scripts.
    /// On-chain verification is now the default for --alert-id — this flag
    /// is a no-op. Use --offline if you want the OLD default (hash-only).
    #[arg(long, hide = true)]
    verify_onchain: bool,

    /// Solana RPC URL for the on-chain anchor check (skip via --offline).
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

/// The engine anchors on-chain ONLY for high-severity transitions
/// (`engine-rs/main.rs`: `PegState::Depeg | Critical | BlackSwan`). Mirror that
/// policy here so the verifier knows when a missing anchor is expected vs a gap.
fn state_expects_anchor(state: PegState) -> bool {
    matches!(
        state,
        PegState::Depeg | PegState::Critical | PegState::BlackSwan
    )
}

/// A `not_applicable` commit_status may be skipped silently ONLY when the
/// receipt's final state is one the engine never anchors on-chain
/// (Pegged / Drift / Unknown — genuinely cost-exempt). A `not_applicable` on a
/// state the anchor policy DOES cover (Depeg / Critical / BlackSwan) means the
/// on-chain commit was disabled when the alert fired (e.g. PEGANA_COMMIT_KEYPAIR
/// unset — the engine writes `not_applicable` "even for critical alerts" in that
/// case, `engine-rs/main.rs`), so the anchor the policy expects is absent. That
/// must NOT pass silently — the caller surfaces it as ONCHAIN_INCOMPLETE (exit 5,
/// ADR-0033), never a false pass and never a tamper exit 4. Every other
/// non-committed status always surfaces regardless of state.
fn onchain_skip_is_ok(commit_status: &str, final_state: PegState) -> bool {
    commit_status == "not_applicable" && !state_expects_anchor(final_state)
}

/// `pending` means an anchor attempt is in flight (ADR-0004's 24h retry
/// window) — distinct from `not_applicable` (exempt, handled by
/// `onchain_skip_is_ok`) and from a real terminal failure
/// (`retry_exhausted` / `wallet_drained` / unrecognized, which still hard-
/// fail as ANCHOR NOT VERIFIED, unchanged).
fn is_onchain_pending(commit_status: &str) -> bool {
    commit_status == "pending"
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

    if cli.verify_onchain && !cli.quiet {
        eprintln!(
            "Note: --verify-onchain is deprecated and now a no-op — on-chain \
             verification runs by default for --alert-id. Pass --offline to skip it."
        );
    }

    let receipt: Receipt = match (&cli.alert_id, &cli.bundle) {
        (Some(id), _) => fetch_bundle(&cli.api_url, *id).await?,
        (None, Some(path)) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read bundle {}", path.display()))?;
            serde_json::from_str(&text).context("parse bundle JSON")?
        }
        (None, None) => bail!("provide either --alert-id <UUID> or --bundle <path>"),
    };

    verify(&receipt, cli.quiet)?; // exits 1 directly on hash/re-derive mismatch

    // On-chain verification is the default for --alert-id (v0.5.0+, was
    // opt-in via --verify-onchain before). --offline skips it explicitly;
    // --bundle mode has no alert_id to look an anchor up with, so it is
    // always offline regardless of the flag.
    match (&cli.alert_id, cli.offline) {
        (Some(id), false) => {
            if let Err(e) =
                verify_onchain(&cli.api_url, &cli.solana_rpc, *id, &receipt, cli.quiet).await
            {
                eprintln!("ONCHAIN_INCOMPLETE  {e:#}");
                eprintln!(
                    "  This is NOT a mismatch — the on-chain check could not be completed \
                     (network/RPC issue, or the anchor isn't committed yet). The hash result \
                     above still stands as tamper-evidence. Retry, check --solana-rpc / \
                     --api-url, or pass --offline to skip this check."
                );
                std::process::exit(5);
            }
        }
        (Some(_), true) => {
            if !cli.quiet {
                eprintln!(
                    "Note: on-chain check skipped (--offline). PASS above is hash-only \
                     tamper-evidence; it does not confirm the on-chain anchor."
                );
            }
        }
        (None, _) => {
            if !cli.quiet {
                eprintln!(
                    "Note: on-chain check not available for --bundle (no alert_id to look \
                     up an anchor for). PASS above is hash-only tamper-evidence."
                );
            }
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
        let final_state = receipt.expected_computed.final_state;
        if onchain_skip_is_ok(status, final_state) {
            if !quiet {
                eprintln!(
                    "Note: no on-chain anchor for this alert (commit_status: {status}) — \
                     expected; {final_state:?} transitions are never anchored on-chain.",
                );
            }
            return Ok(());
        }
        // `not_applicable` on a state the anchor policy DOES cover
        // (Depeg / Critical / BlackSwan): the engine's on-chain commit was
        // disabled when this alert fired (e.g. PEGANA_COMMIT_KEYPAIR unset →
        // `not_applicable` even for critical), so the anchor the policy expects
        // is absent. The off-chain re-derivation already PASSED above; the
        // on-chain leg is INCOMPLETE, not tampered. Bail (→ exit 5
        // ONCHAIN_INCOMPLETE) rather than exit 4, so we never cry tamper on an
        // operator config choice AND never silently pass a severe transition
        // that lacks its policy-expected anchor.
        if status == "not_applicable" {
            bail!(
                "severe transition ({final_state:?}) has no on-chain anchor \
                 (commit_status: not_applicable) — on-chain commit was disabled when \
                 this alert fired, so the anchor the policy expects for this state is \
                 absent. Off-chain re-derivation PASSED; on-chain leg UNVERIFIABLE.",
            );
        }
        // ADR-0004: `pending` means the anchor attempt is inside its 24h
        // retry window — a legitimate transient state for a just-fired
        // alert, NOT tamper evidence. Bail (propagate Err) rather than
        // process::exit(4) here so run()'s handler reports it as
        // ONCHAIN_INCOMPLETE (exit 5), never as ANCHOR NOT VERIFIED.
        if is_onchain_pending(status) {
            bail!(
                "on-chain anchor not committed yet (status: pending, within the 24h \
                 commit window — ADR-0004)"
            );
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
    use super::{
        is_onchain_pending, onchain_signer_ok, onchain_skip_is_ok, state_expects_anchor, verify,
    };
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
            intrinsic_stale: false,
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
        // `not_applicable` is silently exempt ONLY for states the engine never
        // anchors on-chain (Pegged / Drift / Unknown are cost-exempt).
        for state in [PegState::Pegged, PegState::Drift, PegState::Unknown] {
            assert!(
                onchain_skip_is_ok("not_applicable", state),
                "not_applicable on {state:?} (never anchored) must be exempt"
            );
        }
        // `not_applicable` on a state the anchor policy COVERS means the anchor
        // was disabled at emission — it must NOT pass silently (caller surfaces
        // it as ONCHAIN_INCOMPLETE / exit 5, not a false pass).
        for state in [PegState::Depeg, PegState::Critical, PegState::BlackSwan] {
            assert!(
                !onchain_skip_is_ok("not_applicable", state),
                "not_applicable on {state:?} (anchor-expected) must surface, not pass"
            );
        }
        // Every other non-committed status never passes, regardless of state.
        for s in [
            "pending",
            "retry_exhausted",
            "wallet_drained",
            "persistence_failed",
            "unknown",
        ] {
            assert!(
                !onchain_skip_is_ok(s, PegState::Pegged),
                "status {s} must not pass silently"
            );
        }
    }

    #[test]
    fn anchor_policy_matches_engine() {
        // Mirrors engine-rs/main.rs: only Depeg/Critical/BlackSwan are anchored.
        assert!(state_expects_anchor(PegState::Depeg));
        assert!(state_expects_anchor(PegState::Critical));
        assert!(state_expects_anchor(PegState::BlackSwan));
        for state in [PegState::Pegged, PegState::Drift, PegState::Unknown] {
            assert!(
                !state_expects_anchor(state),
                "{state:?} is never anchored on-chain"
            );
        }
    }

    // ── is_onchain_pending: `pending` is transient, never a mismatch ─────────

    #[test]
    fn is_onchain_pending_true_for_pending() {
        assert!(is_onchain_pending("pending"));
    }

    #[test]
    fn is_onchain_pending_false_for_others() {
        for s in [
            "not_applicable",
            "committed",
            "retry_exhausted",
            "wallet_drained",
            "unknown",
        ] {
            assert!(
                !is_onchain_pending(s),
                "status {s} must not read as pending"
            );
        }
    }
}
