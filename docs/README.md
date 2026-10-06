# n2link Documentation

The root [README](../README.md) is the installation and feature overview. These guides cover
operations and security procedures that need more detail than the overview.

## Known issues

- [Known issues and workarounds](ISSUES.md): runtime, build/platform, WASM plugin, Flow Copilot
  and test-suite issues that are known but not yet fixed.

## Operations

- [Credential lifecycle](operations/credential-lifecycle.md): inspect, migrate, rotate, recover,
  export, validate, and roll back encrypted flow credentials.
- [WASM plugins](operations/wasm-plugins.md): enable the optional sandbox (`nodes_wasm`),
  install, upgrade, roll back, observe and troubleshoot third-party plugin nodes.
- [Operational history](operations/history.md): SQLite deploy, node, Copilot, and fleet
  history (`history_sqlite` is in the default app build; recording is `[history] enabled`).

Implementation decisions for Flow Copilot metadata are in [`adoption/phase5/DESIGN.md`](../adoption/phase5/DESIGN.md).

## Development

- [Writing WASM plugins](development/wasm-plugins.md): the Rust guest SDK, manifest schema 1,
  ABI v1, message encoding, testing and versioning. Examples live in
  [`crates/wasm-guest/examples`](../crates/wasm-guest/examples).

## Security

- [Outbound egress policy](security/egress-policy.md): inventory outbound connections, build an
  allowlist, enable enforcement, validate it, and roll back safely.
- [Inbound API and webhook protection](security/ingress-protection.md): configure endpoint-class
  body, header, rate, concurrency, timeout, proxy-trust, response, and rollback boundaries.

Implementation decisions, measured resource costs, test evidence, and phase boundaries are kept
separately under [`adoption/`](../adoption/).
