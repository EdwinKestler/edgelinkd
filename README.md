# EdgeLinkd: Node-RED Reimplemented in Rust

[![Build Status]][actions]
[![GitHub Release]][releases]
[![GitHub Downloads]][releases]

[Build Status]: https://img.shields.io/github/actions/workflow/status/oldrev/edgelinkd/CICD.yml?branch=master
[actions]: https://github.com/oldrev/edgelinkd/actions?query=branch%3Amaster
[GitHub Release]: https://img.shields.io/github/v/release/oldrev/edgelinkd?include_prereleases
[releases]: https://github.com/oldrev/edgelinkd/releases
[GitHub Downloads]: https://img.shields.io/github/downloads/oldrev/edgelinkd/total
![Node-RED Rust Backend](assets/banner.jpg)

English | [简中](README.zh-cn.md)

## Overview

**EdgeLinkd** is a high-performance, memory-efficient Node-RED compatible runtime engine built from the ground up in Rust, now featuring an integrated web UI for complete standalone operation.

**Why EdgeLinkd?**
- **10x less memory usage** than Node-RED (only 10% of Node-RED's memory footprint)
- **Native performance** with Rust's zero-cost abstractions
- **Integrated web interface** - full Node-RED UI built-in for flow design and management
- **Standalone operation** - no external Node-RED installation required
- **Drop-in replacement** - use your existing `flows.json` files
- **Perfect for edge devices** with limited resources
- **Node-RED compatibility** - design, deploy, and run flows all in one application

EdgeLinkd now includes the complete Node-RED web editor, allowing you to design flows directly in the browser while executing them with native Rust performance. You can also run it headless for production deployments on resource-constrained devices.

Only the `function` node uses the lightweight QuickJS JS interpreter to run JavaScript code; all other functionalities are implemented in native Rust code for maximum performance.

## A Short Demo


<video src="https://github.com/user-attachments/assets/5841db63-513a-4b36-8566-57c74adb7b60" controls width="100%"></video>

### Use Cases

- **Flow Development**: Design and test flows directly in the integrated web editor
- **Rapid Prototyping**: Full Node-RED UI for quick flow development and iteration
- **IoT Edge Gateways**: Process sensor data with minimal resource usage
- **Industrial Automation**: Run control flows on embedded controllers with web-based monitoring
- **Home Automation**: Deploy smart home logic on Raspberry Pi with remote web access
- **Development & Production**: Use web UI for development, headless mode for production deployment
- **Cloud-to-Edge Migration**: Move Node-RED flows from cloud to edge with unified interface
- **Container Deployments**: Lightweight containers for edge computing with optional web UI
- **Remote Management**: Access and modify flows remotely through the web interface


## Quick Start

### 0. Clone the Repository

**Clone the repository with submodules:**

```bash
git clone --recursive https://github.com/oldrev/edgelinkd.git
```

Or if you've already cloned without submodules:

```bash
git clone https://github.com/oldrev/edgelinkd.git
cd edgelinkd
git submodule update --init --recursive
```

### 1. Build

**Prerequisites**: Rust 1.80 or later

```bash
cargo build --release
```

**Windows users**: Ensure `patch.exe` is in your PATH (included with Git) and install Visual Studio for MSVC.

**Supported platforms**:

- `x86_64-pc-windows-msvc`
- `x86_64-pc-windows-gnu`
- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `armv7-unknown-linux-gnueabihf`
- `armv7-unknown-linux-gnueabi`

</details>


### 2. Run

**Start EdgeLinkd with integrated web UI (recommended):**

```bash
cargo run --release --
# or after build
./target/release/edgelinkd
```

By default, your browser will open the Node-RED frontend at [http://127.0.0.1:1888](http://127.0.0.1:1888).

**Main command-line options:**

- `[FLOWS_PATH]`: Optional, specify the flow file (default: `~/.edgelinkd/flows.json`)
- `--headless`: Headless mode (no Web UI, suitable for production)
- `--bind <BIND>`: Custom web server bind address (default: `127.0.0.1:1888`)
- `-u, --user-dir <USER_DIR>`: Specify user directory (default: `~/.edgelink`)
- See more options with `--help`

**Examples:**

```bash
# Run in headless mode
./target/release/edgelinkd run --headless

# Specify flow file and port
./target/release/edgelinkd run ./myflows.json --bind 0.0.0.0:8080
```

> All data and configuration are stored in the `~/.edgelink` directory by default.

Use `--help` to see all commands and options:

```bash
./target/release/edgelinkd --help
./target/release/edgelinkd run --help
```

#### Run Unit Tests

```bash
cargo test --all
```

#### Run Integration Tests

Running integration tests requires first installing Python 3.9+ and the corresponding Pytest dependencies:

```bash
pip install -r ./tests/requirements.txt
```

Then execute the following command:

```bash
set PYO3_PYTHON=YOUR_PYTHON_EXECUTABLE_PATH # Windows only
cargo build --all
py.test
```

## Configuration

EdgeLinkd can be configured through command-line arguments and configuration files.

### Web UI Configuration

**Command-line options:**
- `--bind <address>`: Set the web server binding address (default: `127.0.0.1:1888`)
- `--headless`: Run without the web UI for production deployments
- `--user-dir <path>`: Specify custom user directory for flows and settings

**Configuration file:**
You can also configure the web UI through the configuration file (`edgelinkd.toml`):

```toml
[ui-host]
host = "0.0.0.0"
port = 1888
```

A deploy that sends `rev` is rejected with HTTP 409 `version_mismatch` when that rev is not the SHA-256 of the flows on disk. Leaving `rev` out still deploys. `POST /flows/rollback` restores the previous file. One previous copy is kept.

Admin login stays off until `[admin]` in `edgelinkd.toml` sets a password, a `viewer`/`deployer`/`administrator` user, or a complete `[admin.oidc]` section. Viewer is read-only, deployer can edit settings and deploy flows, and administrator can change process configuration. Passwords accept Node-RED-compatible `$2a$`, `$2b$`, and `$2y$` bcrypt hashes. Generate one with `npx node-red-admin hash-pw`; plaintext remains accepted only as a migration path and emits a startup warning. Malformed or unsupported bcrypt-prefixed values stop startup. Bcrypt support is enabled by the default `admin_bcrypt` feature and can be omitted from a minimal build. An unknown role or a half-filled OIDC section also stops the process at startup. `GET /status` reports the flow revision, process uptime, node errors since the engine started, context-key ages, and MQTT or Modbus link text. Fleet push stays off until `[fleet] enabled = true`. The commented examples are in a newly created `edgelinkd.toml`.

### Encrypted credential sidecars

Existing plaintext `flows_cred.json` files remain readable and are never rewritten at startup.
Encryption is an explicit operation. Preview it first, then provide a new empty backup directory:

```bash
EDGELINK_HOME="$PWD" cargo run -- credentials status
EDGELINK_HOME="$PWD" cargo run -- credentials migrate --dry-run
EDGELINK_HOME="$PWD" cargo run -- credentials migrate --backup-dir /offline/edgelink-credential-backup
```

The default application build uses a versioned XChaCha20-Poly1305 envelope. It generates a
private `flows_cred.key` keyring during migration unless `EDGELINK_CREDENTIAL_KEY` contains an
injected 32-byte base64url key. An explicitly configured `credentials.key_env` overrides that
environment-variable name; a non-empty injected key always takes precedence and malformed input
fails closed. Never commit `*.key`, plaintext exports, or backup directories.

Local-key rotation requires a new backup file for the old key. Recovery proves the supplied
keyring decrypts both generations before installing it. Export creates a directly compatible,
mode-`0600` plaintext sidecar pair at the requested path and its `.prev` companion for a
controlled downgrade; it never overwrites either output.

```bash
EDGELINK_HOME="$PWD" cargo run -- credentials rotate --backup-key /offline/old-flows-cred.key
EDGELINK_HOME="$PWD" cargo run -- credentials recover --key-file /offline/old-flows-cred.key
EDGELINK_HOME="$PWD" cargo run -- credentials export --output /offline/flows_cred.json
```

Deploy and rollback preserve whichever sidecar format is already active. A build without the
`credential_encryption` feature still reads plaintext, but rejects an encrypted envelope. Keep
the encrypted installation and its offline key backup until the exported copy has been exercised
successfully with the older runtime.

The complete migration, rotation, recovery, and downgrade procedure is in the
[credential lifecycle operations manual](docs/operations/credential-lifecycle.md).

### Inbound API and webhook protection

Editor/admin, authentication, WebSocket, health, static, Flow Copilot, fleet, and `http in`
traffic have independent finite budgets under `[api_protection]`. Defaults cover request bodies
and headers, per-principal/client rate, global and class concurrency, queue and execution
deadlines, and response size. Forwarded client addresses are ignored unless the direct peer is in
the explicit `trusted_proxies` list. An optional `webhook_bearer_env` requires every `http in`
request to authenticate without placing the token in a configuration file.

Use one class's `mode = "observe"` as a temporary compatibility fallback; the other classes remain
enforced. The [inbound protection runbook](docs/security/ingress-protection.md) lists route classes,
defaults, proxy rules, status codes, acceptance checks, and the configuration-only rollback.

### Outbound network policy

`[egress] mode = "off"` preserves the existing outbound behavior. Use `"observe"` first to
inventory HTTP, AI, OIDC, fleet, MQTT, WebSocket, TCP, UDP, and Modbus decisions without
blocking them. In `"enforce"`, every resolved address must match an exact protocol/port rule;
private DNS targets also require an IP/CIDR constraint. Cloud metadata endpoints remain denied.
Invalid modes, wildcard hosts, malformed CIDRs, zero ports, zero timeouts, and unknown fields
stop startup.

```toml
[egress]
mode = "observe" # migrate to "enforce" after reviewing the decision log
allow_environment_proxy = false
# proxy_url = "http://proxy.example.internal:3128"
connect_timeout_ms = 10000
request_timeout_ms = 60000
idle_timeout_ms = 30000
max_response_bytes = 1048576
max_redirects = 5

# Local industrial MQTT example. Broker credentials stay in the credential sidecar.
[[egress.allow]]
protocols = ["mqtt"]
host = "127.0.0.1"
ports = [1883]

# If proxy_url is enabled, its origin needs its own address-bound rule.
# [[egress.allow]]
# protocols = ["http"]
# host = "proxy.example.internal"
# cidr = "192.168.10.0/24"
# ports = [3128]

# A private DNS name must also be bound to its expected network.
[[egress.allow]]
protocols = ["modbus"]
host = "plc.example.internal"
cidr = "192.168.20.0/24"
ports = [502]
```

In `observe` and `enforce`, ambient environment proxy variables are always isolated. Setting
`allow_environment_proxy = true` in a governed mode is rejected because such a proxy cannot be
pinned. Use an explicit credential-free `proxy_url` instead and add a matching HTTP/HTTPS rule;
the proxy is then a declared trust boundary and its actual socket is resolved, checked, and
pinned. Redirects are resolved and checked again, HTTP/AI/OIDC/fleet responses are bounded, and
policy logs contain only the purpose, protocol, port, action, and reason—not hostnames, URL paths,
queries, credentials, or tokens. Roll back without a data migration by changing the mode to
`observe` or `off` and restarting; flows and credential files are unchanged.

See the [egress policy security runbook](docs/security/egress-policy.md) for a staged rollout,
acceptance checks, and rollback procedure.

#### Editor configuration pane

An administrator can manage the egress policy from **User Settings → EdgeLinkd**. The pane reads,
validates, saves, applies, and rolls back only the `[egress]` table in the active environment
overlay (`edgelinkd.dev.toml` by default). It never returns the rest of that file to the browser,
because it may contain passwords or OIDC secrets. Saves use a SHA-256 revision, atomic `0600`
writes, and one `.prev` copy. Apply replaces the shared policy and restarts the flow runtime; an
activation failure restores the previous policy and file.

The pane is off by default and cannot be enabled without authentication:

```toml
[config_editor]
enabled = true

[[admin.users]]
username = "operator"
# Paste the output of: npx node-red-admin hash-pw
password = "change-me"
role = "administrator"
```

The backend independently enforces `config.read`, `config.write`, and `runtime.restart`. The
`deployer` role deliberately lacks those permissions. Bootstrap and recovery remain file-based so
an editor session cannot grant itself administrator access.

## Project Status

**Alpha Stage**: The project is currently in the *alpha* stage and cannot guarantee stable operation.

**New: Integrated Web UI**: EdgeLinkd now includes a complete Node-RED web interface for flow design and management. The web UI is fully compatible with Node-RED's editor and provides the same user experience while running on the high-performance Rust runtime.

**Web UI Features**:
- ✅ Complete Node-RED editor interface
- ✅ Flow design and editing
- ✅ Node palette with all supported nodes  
- ✅ Deploy flows directly from the browser
- ✅ Real-time flow execution monitoring
- ✅ Debug panel integration
- ✅ Settings and configuration management
- ✅ Import/Export flows functionality

The heavy check mark ( :heavy_check_mark: ) below indicates that this feature has passed the integration test ported from Node-RED.

### Node-RED Features Roadmap:

- [x] :heavy_check_mark: Flow
- [x] :heavy_check_mark: Sub-flow
- [x] Group
- [x] :heavy_check_mark: Environment Variables
- [x] Context
    - [x] Memory storage
    - [x] Local file-system storage
- [x] :heavy_check_mark: RED.util
    - [x] The whole `@node-red/util` surface on the function node sandbox object
      (`getMessageProperty`/`setMessageProperty`, `evaluateNodeProperty`,
      `normalisePropertyExpression`, `normaliseNodeTypeName`, `compareObjects`,
      `ensureString`/`ensureBuffer`, `parseContextStore`, `getSetting`, `encodeObject`, ...),
      covered by the ported upstream spec in `tests/util/test_util.py`
    - [ ] `ensureBuffer()` returns a `Uint8Array`: the sandbox has no Node.js `Buffer`
    - [ ] `prepareJSONataExpression()` / `evaluateJSONataExpression()` fail with `NOT_SUPPORTED`:
      JSONata is implemented by the Rust runtime and is not exposed to JavaScript
    - [ ] `evaluateNodeProperty(v, "date")` with a format string fails with `NOT_SUPPORTED`:
      the sandbox has no `moment` to format with
- [x] Plug-in subsystem[^1]
- [x] JSONata (via the pure-Rust `jsonata-core` engine)
    - [x] `$flowContext()`, `$globalContext()`, `$env()`, `$clone()`, `$I`/`$N` bindings
    - [x] `change` / `switch` / `inject` properties, and environment variables
    - [ ] `$moment()` — an expression calling it fails with an error instead of a value

[^1]: Rust's Tokio async functions cannot call into dynamic libraries, so currently, we can only use statically linked plugins. I will evaluate the possibility of adding plugins based on WebAssembly (WASM) or JavaScript (JS) in the future.

### The Current Status of Nodes:

Refer [REDNODES-SPECS-DIFF.md](tests/REDNODES-SPECS-DIFF.md) to view the details of the currently implemented nodes that comply with the Node-RED specification tests.

- Core nodes:
    - Common nodes:
        - [x] :heavy_check_mark: Console-JSON (For integration tests)
        - [x] :heavy_check_mark: Inject
        - [x] Debug (WIP)
        - [x] :heavy_check_mark: Complete
        - [x] :heavy_check_mark: Catch
        - [x] :heavy_check_mark: Status
        - [x] :heavy_check_mark: Link In
        - [x] :heavy_check_mark: Link Call
        - [x] :heavy_check_mark: Link Out
        - [x] :heavy_check_mark: Comment (Ignored automatically)
        - [x] GlobalConfig (WIP)
        - [x] :heavy_check_mark: Unknown
        - [x] :heavy_check_mark: Junction
    - Function nodes:
        - [x] Function (WIP)
            - [x] Basic functions
            - [x] `node` object (WIP)
            - [x] `context` object
            - [x] `flow` object
            - [x] `global` object
            - [x] `RED.util` object
            - [x] `env` object
        - [x] State
            - [x] JSON table of named states. The first matching edge wins, and the output sets `msg.state`
            - [x] Compare a message property, or a flow, global, or node context key (`eq` / `neq`)
            - [x] `msg.tick`, or `period` in milliseconds, re-checks the current state's edges
            - [x] Entry actions write context keys
            - [ ] Parallel branches, history states, JSONata actions, and a chart editor
        - [x] AI (`nodes_ai`, included by default; disable with `--no-default-features`)
            - [x] `ai-provider` config: OpenAI, xAI (Grok), Anthropic (Claude), Snowflake Cortex
            - [x] `ai-chat` one-shot and `msg.messages` conversations. Keys stay in `flows_cred.json`
            - [x] Flow Copilot add-and-connect draft/validate/apply; review and Deploy stay manual
            - [ ] Flow Copilot update/delete/install tools
            - [ ] `ai-agent` bounded tool loop
        - [x] Scan (`runtime_scan`, off by default)
            - [x] One task writes `flow.scan` (`seq`, `period`, `duration`, `overrun`). Nodes read it
            - [x] `runtime.scan.period_ms` in `edgelinkd.toml` is the period in milliseconds. Absent or `0` leaves the task off. A period below 10 ms is an error at start
            - [x] When a body exceeds the period, the next scan sets `overrun` and the `scan` node status turns red. A later body within the period clears it
            - [x] Soft real-time on the host OS. One scan. Overrun is visible. This is not a worst-case latency bound
        - [x] :heavy_check_mark: Switch
        - [x] :heavy_check_mark: Change
        - [x] :heavy_check_mark: Range
        - [x] :heavy_check_mark: Template
        - [x] Delay
        - [x] Trigger
        - [x] Exec
        - [x] :heavy_check_mark: Filter (RBE)
    - Network nodes:
        - [x] MQTT In
        - [x] MQTT Out
        - [ ] MQTT Broker
            - [x] In and out use this node for host, port, client id, keepalive, clean session, will, birth, and close. An empty or unknown broker id is an error at deploy
            - [ ] Version 5 is MQTT 5.0 on TCP for the mapped connect, subscribe, and publish fields. MQTT 3.1, TLS, WebSocket, and enhanced AUTH are not supported
            - [ ] Upstream send and receive specs were run against RabbitMQ 3.13.7 on 127.0.0.1:1883. QoS 2 comes back as QoS 1. A 2000-second message expiry is the broker's remaining time and can come back as 1999. Buffer inputs, a forced will drop, and the two JS load tests are not asserted
        - [x] Modbus TCP (`nodes_modbus`, off by default)
            - [x] Coils, discrete inputs, holding registers, and input registers map to one context key each. A forced key is not overwritten by a read, and a write sends the forced value
            - [x] Function codes 1 through 6, Modbus TCP, one value at a time. A period below 10 ms is rejected
            - [ ] Serial RTU and other Modbus classes
        - [x] HTTP In
        - [x] HTTP Out
        - [x] HTTP Request
        - [x] WebSocket Listener
        - [x] WebSocket Client
        - [x] WebSocket In
        - [x] WebSocket Out
        - [x] TCP In
        - [x] TCP Out
        - [x] TCP Get
        - [x] UDP In
        - [x] :heavy_check_mark: UDP Out
            - [x] Unicast
            - [x] Multicast
        - [x] TLS (WIP)
        - [x] HTTP Proxy (WIP)
    - Sqeuence nodes:
        - [x] Split
        - [x] Join
        - [x] Sort
        - [x] Batch
    - Parse nodes:
        - [x] CSV
        - [ ] HTML
        - [x] :heavy_check_mark: JSON
        - [x] :heavy_check_mark: XML
        - [x] YAML
    - Storage
        - [x] File
        - [x] File In
        - [x] Watch

## Roadmap

Check out our [milestones](https://github.com/oldrev/edgelinkd/milestones) to get a glimpse of the upcoming features and milestones.

## Contribution

![Alt](https://repobeats.axiom.co/api/embed/cd18a784e88be20d79778703bda8858523c4257e.svg "Repobeats analytics image")

We welcome contributions! Whether it's:

- **Bug reports** and feature requests
- **Documentation** improvements
- **Code contributions** and new node implementations
- **Testing** on different platforms

> Note: Please make meaningful contributions, or watch and learn. Simply modifying the README or making non-substantive changes will be considered malicious behavior.

Please read [CONTRIBUTING.md](.github/CONTRIBUTING.md) for details.

### Support the Project

If EdgeLinkd saves you memory and improves your edge deployments, consider supporting development:

<a href='https://ko-fi.com/O5O2U4W4E' target='_blank'><img height='36' style='border:0px;height:36px;' src='https://storage.ko-fi.com/cdn/kofi3.png?v=3' border='0' alt='Buy Me a Coffee at ko-fi.com' /></a>

[![Support via PayPal.me](assets/paypal_button.svg)](https://www.paypal.me/oldrev)

## Known Issues

Please refer to [ISSUES.md](docs/ISSUES.md) for a list of known issues and workarounds.

## Feedback and Support

We welcome your feedback! If you encounter any issues or have suggestions, please open an [issue](https://github.com/edge-link/edgelinkd/issues).

* Contact me: E-mail: oldrev(at)gmail.com
* Discord: [https://discord.gg/XJstgANe26](https://discord.gg/XJstgANe26)

## License

This project is licensed under the Apache 2.0 License - see the [LICENSE](LICENSE) file for more details.

Copyright © Li Wei and other contributors. All rights reserved.
