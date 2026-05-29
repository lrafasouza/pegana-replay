# pegana-replay

Open math + CLI behind every Pegana alert.

[pegana.xyz](https://www.pegana.xyz) is a peg-risk oracle for Solana. It watches
~27 LSTs, stablecoins, yield-bearing wrappers, CDPs, and synthetic-leverage
tokens, and publishes a state transition the moment an asset moves out of
PEGGED. Every alert ships with a **content-addressed receipt** — this repo is
how anyone outside Pegana can reproduce that receipt offline and confirm the
math.

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
#    pre-built binary for darwin-aarch64 today; v0.1.0 GA fans out to
#    linux-x86_64, darwin-x86_64, and windows-x86_64 with Sigstore.)
curl --proto '=https' --tlsv1.2 -LsSf \
  https://releases.pegana.xyz/pegana-replay-installer.sh | sh

# 2. Fetch the bundle and replay. The CLI re-applies the same code in
#    crates/methodology against the receipt's frozen inputs and compares
#    the resulting hash byte-for-byte.
pegana-replay --alert-id 0190ab12-3456-7890-abcd-ef0123456789

# 3. Verify the on-chain commitment too (the engine commits each receipt's
#    sha256 as a Solana SPL Memo via the ops wallet).
pegana-replay --alert-id 0190ab12-3456-7890-abcd-ef0123456789 --verify-onchain

# 4. (After v0.1.0 GA) verify the binary's provenance via the Sigstore
#    attestation issued by this repo's CI.
gh attestation verify ~/.pegana/bin/pegana-replay \
  --owner lrafasouza --repo pegana-replay
```

## Versioning

- `v*` tags fan out the full release matrix (4 OS/arch combos, all signed).
- `methodology-v*` tags pin a specific methodology version (semver) — receipts
  reference this exact tag in their `methodology_version` field. The
  `/audit/<id>` page on the production site links back here at the matching
  tag so anyone reading a 6-month-old receipt sees the methodology *as it
  was at the time*, not as it is today.

## License

MIT. See `LICENSE`.

## Links

- Site & live audit ledger: [www.pegana.xyz](https://www.pegana.xyz)
- Self-hosted install mirror: [releases.pegana.xyz](https://releases.pegana.xyz)
- Telegram for issues / feedback: [@PeganaWatchBot](https://t.me/PeganaWatchBot)
- Twitter / X: [@peganaxyz](https://x.com/peganaxyz)
