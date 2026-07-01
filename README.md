# pegana-replay

Open math + CLI behind every Pegana alert.

[pegana.xyz](https://www.pegana.xyz) is a peg-risk oracle for Solana. It watches
~27 LSTs, stablecoins, yield-bearing wrappers, CDPs, and synthetic-leverage
tokens, and publishes a state transition the moment an asset moves out of
PEGGED. Every alert ships with a **content-addressed receipt** — this repo is
how anyone outside Pegana can verify a Pegana alert's canonical receipt hash
and its signer-pinned on-chain anchor, confirming the published history wasn't
altered (tamper-evidence).

## Verify a Pegana alert in 60 seconds

**Option A — install the CLI (crates.io):**
```sh
cargo install pegana-replay
pegana-replay --alert-id <UUID>     # an alert id from pegana.xyz or the API
# → PASS  receipt_sha256 matches   (tamper-evidence: the published history wasn't altered)
```
Or verify a saved bundle offline: `pegana-replay --bundle <path-to-replay-bundle.json>`.

**Option B — no install, verify in your browser:**
Open the audit page for any alert — `https://www.pegana.xyz/audit/<alert-id>` — where the in-browser
verifier (`ReceiptVerifier`) recomputes the canonical receipt hash in pure JS (byte-identical to the
CLI), trusting nothing from our servers.

**What PASS means:** the `receipt_sha256` you recomputed equals the published one → the verdict and its
inputs were not altered after the fact. For schema-v2 receipts the CLI also re-derives the verdict from
the frozen inputs (not just the hash).

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://releases.pegana.xyz/pegana-replay-installer.sh | sh

pegana-replay --alert-id <UUID>
# → PASS  receipt_sha256 matches
```

## What's in this repo

| Path | What it is |
|---|---|
| `crates/methodology/` | Pure functions: spread, EWMA, hysteresis, transition. The exact code the engine runs in production. |
| `crates/pegana-replay-cli/` | The CLI binary that downloads a receipt, reapplies the methodology, and reports PASS / FAIL. |
| `crates/common-verify/` | Config types + the AlertEvidence schema returned by `/v1/audit/:id`. |
| `assets.toml` | Canonical asset list. Symbol, mint, class, peg target, threshold bands, intrinsic + market strategies. Hashed into every receipt. |
| `docs/adr/` | Architecture Decision Records — every load-bearing methodology choice is dated and rationalised here. |
| `.github/workflows/release.yml` | `cargo-dist` pipeline that publishes signed binaries to GitHub Releases on every `v*` tag, with Sigstore attestations via GitHub-issued OIDC. |

## What's NOT in this repo

The engine, dispatcher, indexer, API server, MCP server, Telegram bot,
on-chain commit pipeline, and webhook signing primitives live in a closed
sibling repo (`lrafasouza/pegana`, private). Closed for operational reasons —
**every decision they produce, however, is reproducible from this repo**
against the alert's audit receipt URL.

This repo is automatically mirrored from the private one by a path-filtered
subtree-push action; do not file PRs against it directly (see CONTRIBUTING).

## Verify a specific alert end-to-end

```sh
# 1. Install the verifier (no Rust toolchain needed — curl|sh ships a
#    pre-built, Sigstore-attested binary for linux-x86_64 and
#    darwin-aarch64. Intel Mac and Windows build from source via
#    `cargo install --git`.)
curl --proto '=https' --tlsv1.2 -LsSf \
  https://releases.pegana.xyz/pegana-replay-installer.sh | sh

# 2. Fetch the bundle and verify. The CLI re-hashes the receipt's frozen
#    inputs + recorded verdict and compares to the stored canonical hash
#    (tamper-evidence — it does NOT re-execute the methodology).
pegana-replay --alert-id 0190ab12-3456-7890-abcd-ef0123456789

# 3. Verify the on-chain commitment too (the engine commits each receipt's
#    sha256 as a Solana SPL Memo via the ops wallet).
pegana-replay --alert-id 0190ab12-3456-7890-abcd-ef0123456789 --verify-onchain

# 4. Verify the binary's provenance via the Sigstore attestation issued
#    by this repo's CI (linux-x86_64 / darwin-aarch64 release binaries).
gh attestation verify ~/.pegana/bin/pegana-replay \
  --repo lrafasouza/pegana-replay
```

## Versioning

- The CLI **embeds exactly one methodology version**, and its release tag is
  **decoupled** from it: e.g. CLI `v0.4.1` embeds methodology `0.4.0` (the CLI
  can ship fixes without a methodology bump). What matters for replay is the
  **embedded methodology version**, not the CLI tag. A receipt carries the
  `methodology_version` it was produced under, so anyone reading an old receipt
  installs the CLI build that embeds *that* methodology and replays against it
  *as it was at the time*, not as it is today. (The CLI enforces this: a receipt
  whose `methodology_version` differs from the binary's **embedded** methodology
  version exits `3` VERSION_MISMATCH with a hint to install the matching build.)
- Each `v*` tag builds two Sigstore-attested targets — `x86_64-unknown-linux-gnu`
  and `aarch64-apple-darwin`. Intel Mac and Windows build from source.

## License

MIT. See `LICENSE`.

## Links

- Site & live audit ledger: [www.pegana.xyz](https://www.pegana.xyz)
- Self-hosted install mirror: [releases.pegana.xyz](https://releases.pegana.xyz)
- Telegram for issues / feedback: [@PeganaWatchBot](https://t.me/PeganaWatchBot)
- Twitter / X: [@peganaxyz](https://x.com/peganaxyz)
