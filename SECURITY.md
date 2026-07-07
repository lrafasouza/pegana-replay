# Security Policy

This repo is the verification layer behind every Pegana alert receipt
(canonical hashing, on-chain anchor checks, methodology replay). A bug here
could let a tampered receipt verify as clean, so we treat reports against it
seriously.

## Reporting a vulnerability

Report privately via Telegram to [@PeganaWatchBot](https://t.me/PeganaWatchBot)
before any public disclosure — same channel documented in `CONTRIBUTING.md`.
Please do not open a public GitHub issue for a suspected vulnerability.

Include, if possible:
- The affected crate/file and a minimal reproduction (a receipt, alert ID, or
  input that triggers the issue)
- Whether the bug affects verification correctness (a tampered receipt passes)
  or availability (a crash/panic on adversarial input) — these are triaged
  differently

## Scope

In scope: `crates/methodology`, `crates/pegana-replay-cli`,
`crates/common-verify` — the code in this repository.

Out of scope: the private engine, API, indexer, bot, and web app that produce
the receipts this CLI verifies. Report issues with pegana.xyz itself the same
way, via Telegram.
