use crate::Result;

/// Creates a default flows.json with an inject->debug flow that sends "Hello, World" every 5 seconds
pub fn create_default_flows_json() -> serde_json::Value {
    serde_json::json!([
    {
        "id": "adf5c374d9ac0466",
        "type": "tab",
        "label": "Flow 1"
    },
    {
        "id": "ded1a8c84fec2323",
        "type": "inject",
        "z": "adf5c374d9ac0466",
        "name": "Inject Hello",
        "props": [
            {
                "p": "payload"
            },
            {
                "p": "topic",
                "vt": "str"
            }
        ],
        "repeat": "5",
        "crontab": "",
        "once": false,
        "onceDelay": 0.1,
        "topic": "",
        "payload": "Hello, EdgeLinkd!",
        "payloadType": "date",
        "x": 410,
        "y": 280,
        "wires": [
            [
                "dc18e4d63818b44b"
            ]
        ]
    },
    {
        "id": "dc18e4d63818b44b",
        "type": "debug",
        "z": "adf5c374d9ac0466",
        "name": "debug 1",
        "active": true,
        "tosidebar": true,
        "console": true,
        "tostatus": false,
        "complete": "payload",
        "targetType": "msg",
        "statusVal": "",
        "statusType": "auto",
        "x": 670,
        "y": 280,
        "wires": []
    }
    ])
}

/// Creates a default edgelinkd.toml configuration file
pub fn create_default_config_file(config_dir: &str) -> Result<()> {
    use std::fs;
    use std::path::Path;

    let config_path = Path::new(config_dir).join("edgelinkd.toml");

    // If config file already exists, nothing to do
    if config_path.exists() {
        return Ok(());
    }

    // Create directory if it doesn't exist
    if !Path::new(config_dir).exists() {
        fs::create_dir_all(config_dir)?;
        log::info!("Created config directory: {config_dir}");
    }

    // Create default config file content
    let default_config = r#"[runtime]

[runtime.engine]

[runtime.context]
default = "memory"

[runtime.context.stores]
memory = { provider = "memory" }

[runtime.flow]
node_msg_queue_capacity = 16
# How many messages a node may keep buffered while working on a message sequence; 0 means no limit.
# This is the equivalent of Node-RED's `nodeMessageBufferMaxLength` settings.js property.
node_message_buffer_max_length = 0
# How many messages the TCP nodes may queue while a connection is busy; the oldest is dropped
# once the queue is full. This is Node-RED's `tcpMsgQueueSize` settings.js property.
tcp_msg_queue_size = 1000

[runtime.scan]
# Soft real-time scan. Compiled only with the `runtime_scan` feature, which is off by default.
# Period in milliseconds. 0 disables the task. A value below 10 is an error at start.
# The floor is the shortest interval the runtime will schedule, not a latency guarantee.
period_ms = 0

[ui-host]
host = "127.0.0.1"
port = 1888

# Outbound compatibility mode. Move to "observe" to inventory decisions, then add exact
# allow rules before selecting "enforce". Ambient environment proxy variables are isolated in
# governed modes; configure a proxy_url and allowlist its origin if a proxy is required.
[egress]
mode = "off"
allow_environment_proxy = false
# proxy_url = "http://proxy.example.internal:3128"
connect_timeout_ms = 10000
request_timeout_ms = 60000
idle_timeout_ms = 30000
max_response_bytes = 1048576
max_redirects = 5

# Example for a local MQTT broker. Credentials remain in flows_cred.json, never here.
# [[egress.allow]]
# protocols = ["mqtt"]
# host = "127.0.0.1"
# ports = [1883]

# The editor configuration pane is disabled until explicitly enabled. It also requires admin
# authentication; EdgeLinkd refuses to start if this is true while login is off.
[config_editor]
enabled = false

# Credential sidecars remain plaintext until an explicit `credentials migrate` command. Once
# migrated, this environment variable takes precedence over the local flows_cred.key keyring.
# Never place the key value in this file or commit a generated *.key file.
[credentials]
key_env = "EDGELINK_CREDENTIAL_KEY"
# key_file = "flows_cred.key"

# Admin login is off until a password, a user list, or an OIDC issuer is set.
# A whitespace password does not turn it on. viewer can read, deployer can deploy flows, and
# administrator can change process configuration.
# An unknown role, or an incomplete OIDC section, stops the process at startup.
# [admin]
# password = ""
# [[admin.users]]
# username = "operator"
# Generate with: npx node-red-admin hash-pw
# password = "change-me"
# role = "deployer"
# [[admin.users]]
# username = "viewer"
# password = "change-me"
# role = "viewer"
# [[admin.users]]
# username = "administrator"
# password = "change-me"
# role = "administrator"
# [admin.oidc]
# issuer = "https://idp.example/realms/plant"
# client_id = "edgelinkd"
# client_secret = "change-me"
# role_claim = "edgelink_role"
# redirect_url = "http://127.0.0.1:1888/auth/strategy/callback"

# Fleet push is off until enabled. Devices can also live in fleet.json beside flows.json.
# [fleet]
# enabled = false
"#;

    fs::write(&config_path, default_config)?;
    log::info!("Created default config file at: {}", config_path.display());

    Ok(())
}
