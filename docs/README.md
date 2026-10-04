# EdgeLinkd Documentation

The root [README](../README.md) is the installation and feature overview. These guides cover
operations and security procedures that need more detail than the overview.

## Operations

- [Credential lifecycle](operations/credential-lifecycle.md): inspect, migrate, rotate, recover,
  export, validate, and roll back encrypted flow credentials.
- [Operational history](operations/history.md): optional SQLite deploy, node, Copilot, and
  fleet history behind `history_sqlite`.

## Security

- [Outbound egress policy](security/egress-policy.md): inventory outbound connections, build an
  allowlist, enable enforcement, validate it, and roll back safely.
- [Inbound API and webhook protection](security/ingress-protection.md): configure endpoint-class
  body, header, rate, concurrency, timeout, proxy-trust, response, and rollback boundaries.

Implementation decisions, measured resource costs, test evidence, and phase boundaries are kept
separately under [`adoption/`](../adoption/).
