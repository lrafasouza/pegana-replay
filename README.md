# This repository has moved

**→ [github.com/PeganaHQ/ReplayCLI](https://github.com/PeganaHQ/ReplayCLI)**

`pegana-replay` — the open math and offline verifier behind every
[Pegana](https://www.pegana.xyz) peg-risk receipt — now lives in the
**PeganaHQ** organisation. This repository is archived and read-only.

## What you should do

```sh
# Install the current CLI
cargo install pegana-replay --locked
# or
curl --proto '=https' --tlsv1.2 -LsSf \
  https://releases.pegana.xyz/pegana-replay-installer.sh | sh
```

- **Source, issues, releases:** https://github.com/PeganaHQ/ReplayCLI
- **crates.io:** https://crates.io/crates/pegana-replay

## If you are still running v0.4.4 or earlier

Binaries released here (`v0.1.0` … `v0.4.4`) carry Sigstore build provenance
attested to **this** repository, so they verify with:

```sh
gh attestation verify <binary> --repo lrafasouza/pegana-replay
```

That keeps working — this repo is archived, not deleted. But please upgrade:
`v0.5.0+` is attested to `PeganaHQ/ReplayCLI` and is the only version that
receives fixes.

---

MIT. Archived 2026-07-30.
