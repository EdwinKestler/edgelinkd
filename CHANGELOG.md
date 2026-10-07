# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0-rc.1]

First release under the n2link name. Derived from EdgeLinkd; see [NOTICE](NOTICE).
Known limitations and workarounds: [docs/ISSUES.md](docs/ISSUES.md).

### Verification

Snapshot at 2026-10-06; rerun before tagging if `master` has moved.

- Live evidence on `b39461c` (`artifacts/live-evidence/20261006T203854Z-b39461c/`): 50 checks passed, 0 failed. Pytest in that run: 826 passed, 217 skipped.
- GitHub Actions CICD run 5 on `ab261f3` (Linux): success. The three `scripts/` commits after that SHA did not retrigger CICD. Windows and ARM jobs run only on `schedule` or `workflow_dispatch`.

### Added

- Outbound egress policy (`off` / `observe` / `enforce`) with host and CIDR allowlists, timeouts, and body limits. Default is `off`. Manual: [docs/security/egress-policy.md](docs/security/egress-policy.md).
- Inbound API and webhook budgets under `[api_protection]` (body, header, rate, concurrency, deadlines, trusted proxies). Manual: [docs/security/ingress-protection.md](docs/security/ingress-protection.md).
- Explicit credential encryption lifecycle: `n2linkd credentials status|migrate|rotate|recover|export`. Envelope is XChaCha20-Poly1305; startup never migrates plaintext. Manual: [docs/operations/credential-lifecycle.md](docs/operations/credential-lifecycle.md).
- Optional SQLite operational history (`history_sqlite` in the default build). Recording stays off until `[history] enabled = true`. Manual: [docs/operations/history.md](docs/operations/history.md).
- Flow Copilot add-and-connect drafts with catalog port names and advisory payload types (metadata schema 2). Update, delete, and install tools are not in this release.
- AI nodes in the default binary: `ai-provider`, `ai-chat`, `ai-split`, `ai-structured`. `ai-embed` and `ai-agent` are compiled in and stay experimental until the live OpenAI/xAI and OpenAI/Anthropic runs in [adoption/phase6/LIVE.md](adoption/phase6/LIVE.md).
- Experimental WASM plugin host behind `--features nodes_wasm` (off in default, `full`, and `--no-default-features` builds): Wasmi sandbox, atomic plugin store, `n2linkd plugin` CLI, admin API, Rust guest SDK, examples `uppercase`, `csvparse`, and `tofsense`. Manuals: [docs/operations/wasm-plugins.md](docs/operations/wasm-plugins.md), [docs/development/wasm-plugins.md](docs/development/wasm-plugins.md).
- `scripts/live-evidence.sh` for a recorded live acceptance run.

### Changed

- The daemon, home, config stem, and environment prefix are `n2linkd`, `~/.n2linkd`, `n2linkd.toml`, and `N2LINK_*`.
- Encrypted credential envelopes use format `n2link-credentials`. New installs still write plaintext sidecars until `n2linkd credentials migrate`.
- WASM plugins import `n2link:node/v1` and carry an `n2link.manifest` section. ABI version stays 1. Rebuild guests with `n2link-wasm-guest`.
- Editor chrome uses the n2link title, logo, and favicon. `localStorage` keys are `n2linkd.client.*`. A previous `edgelinkd.client.*` value is read once and rewritten.
- Default `n2linkd` features: `credential_encryption`, `history_sqlite`, `nodes_ai`, `nodes_ai_text`, `nodes_ai_embeddings`, `nodes_ai_agent`, `nodes_postgres`, `nodes_redis`, `admin_bcrypt`. `nodes_wasm`, `nodes_modbus`, and `runtime_scan` stay off.

### Deprecated

These aliases work through 0.4.x, warn once, and go away in the next minor:

- `edgelinkd` launcher in release archives (prints the warning and execs `n2linkd`).
- `~/.edgelinkd` when `~/.n2linkd` is absent. n2link does not move that directory; the warning includes the `mv` command.
- `edgelinkd.toml` (and `.dev` / `.prod`) when the `n2linkd` file is absent. The new name wins as soon as it exists.
- `EDGELINK_HOME`, `EDGELINK_CREDENTIAL_KEY`, and `EDGELINK_RUN_ENV`. If the matching `N2LINK_*` variable is also set to a different value, startup fails.

### Removed

- Dual-read of EdgeLinkd encrypted sidecars (`edgelink-credentials`). Startup and `credentials status` fail with `credentials were encrypted by EdgeLinkd, which n2link cannot read; remove flows_cred.json and flows_cred.key, then re-enter the credentials`. Plaintext `flows_cred.json` still loads.
- Any alias for the old WASM plugin interface (`edgelink:node/v1`, `edgelink.manifest`). Staging a package built for EdgeLinkd fails with a rebuild hint.

### Security

- An `edgelink-credentials` envelope is refused; n2link never decrypts it or returns empty credentials in its place.
