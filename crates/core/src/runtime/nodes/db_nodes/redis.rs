//! Redis config + PING/GET/SET/DEL. TLS is out of scope.

use std::sync::Arc;
use std::time::Duration;

use redis::AsyncCommands;
#[allow(deprecated)]
use redis::aio::Connection;
use serde::Deserialize;
use serde_json::Value;

use crate::N2linkError;
use crate::runtime::egress::{EgressPolicyHandle, EgressPurpose, NetworkProtocol};
use crate::runtime::engine::Engine;
use crate::runtime::flow::Flow;
use crate::runtime::model::json::RedFlowNodeConfig;
use crate::runtime::model::json::RedGlobalNodeConfig;
use crate::runtime::model::{MsgHandle, Variant};
use crate::runtime::nodes::*;
use n2link_macro::*;

const DEFAULT_PORT: u16 = 6379;
const DEFAULT_TIMEOUT_MS: u64 = 5_000;

crate::node_hints!("redis-config", secrets = ["user", "password"], caps = ["network"]);
crate::node_hints!("redis", refs = ["redis" => "redis-config"], caps = ["network"]);

#[global_node("redis-config", red_name = "redis-config")]
struct RedisConfigNode {
    base: BaseGlobalNodeState,
    settings: RedisSettings,
    egress: EgressPolicyHandle,
}

#[derive(Clone, Debug)]
struct RedisSettings {
    host: String,
    port: u16,
    db: i64,
    password: String,
    timeout: Duration,
}

fn json_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned)
}

fn resolve_settings(value: &Value) -> crate::Result<RedisSettings> {
    if value.get("tls").is_some_and(|item| item != &Value::Bool(false))
        || json_string(value, "tls").is_some_and(|mode| mode != "disable")
    {
        return Err(N2linkError::NotSupported("Redis TLS is not supported".to_owned()));
    }
    let host = json_string(value, "host").unwrap_or_else(|| "127.0.0.1".to_owned());
    let port = value.get("port").and_then(Value::as_u64).unwrap_or(DEFAULT_PORT as u64);
    if !(1..=65535).contains(&port) {
        return Err(N2linkError::invalid_operation("redis port is out of range"));
    }
    let db = value.get("db").and_then(Value::as_i64).unwrap_or(0);
    if !(0..=15).contains(&db) {
        return Err(N2linkError::invalid_operation("redis db must be 0..=15"));
    }
    let password = json_string(value, "password")
        .or_else(|| value.get("credentials").and_then(|creds| json_string(creds, "password")))
        .unwrap_or_default();
    let timeout_ms = value.get("timeoutMs").and_then(Value::as_u64).unwrap_or(DEFAULT_TIMEOUT_MS);
    if !(100..=120_000).contains(&timeout_ms) {
        return Err(N2linkError::invalid_operation("redis timeoutMs is out of range"));
    }
    Ok(RedisSettings { host, port: port as u16, db, password, timeout: Duration::from_millis(timeout_ms) })
}

impl RedisConfigNode {
    fn build(
        engine: &Engine,
        config: &RedGlobalNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn GlobalNodeBehavior>> {
        let settings = resolve_settings(&config.rest)?;
        Ok(Box::new(Self {
            base: BaseGlobalNodeState {
                id: config.id,
                name: config.name.clone(),
                type_str: "redis-config",
                ordering: config.ordering,
                context: engine.get_context_manager().new_context(engine.context(), config.id.to_string()),
                disabled: config.disabled,
            },
            settings,
            egress: engine.egress_policy().clone(),
        }))
    }
}

#[async_trait::async_trait]
impl GlobalNodeBehavior for RedisConfigNode {
    fn get_base(&self) -> &BaseGlobalNodeState {
        &self.base
    }
}

fn config_from_flow(flow: &Flow, id: &str) -> crate::Result<(RedisSettings, EgressPolicyHandle)> {
    if id.is_empty() {
        return Err(N2linkError::invalid_operation("redis node has no redis-config"));
    }
    let eid: crate::runtime::model::ElementId =
        id.parse().map_err(|_| N2linkError::invalid_operation("redis-config id is not a node id"))?;
    let engine = flow.engine().ok_or_else(|| N2linkError::invalid_operation("redis node has no engine"))?;
    let global = engine
        .find_global_node_by_id(&eid)
        .ok_or_else(|| N2linkError::invalid_operation(&format!("redis-config '{id}' was not loaded")))?;
    let node = global
        .as_any()
        .downcast_ref::<RedisConfigNode>()
        .ok_or_else(|| N2linkError::invalid_operation(&format!("node '{id}' is not a redis-config")))?;
    Ok((node.settings.clone(), node.egress.clone()))
}

#[derive(Debug, Deserialize)]
struct CommandConfig {
    #[serde(default)]
    redis: String,
    #[serde(default)]
    command: String,
    #[serde(default)]
    key: String,
}

#[flow_node("redis", red_name = "redis", inputs = 1, outputs = 1)]
struct RedisNode {
    base: BaseFlowNodeState,
    redis: String,
    command: String,
    key: String,
}

impl RedisNode {
    fn build(
        _flow: &Flow,
        base_node: BaseFlowNodeState,
        config: &RedFlowNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        let raw = CommandConfig::deserialize(&config.rest)?;
        if raw.redis.trim().is_empty() {
            return Err(N2linkError::invalid_operation("redis node requires a redis-config"));
        }
        let command = if raw.command.trim().is_empty() { "ping".to_owned() } else { raw.command.to_ascii_lowercase() };
        match command.as_str() {
            "ping" | "get" | "set" | "del" => {}
            other => {
                return Err(N2linkError::NotSupported(format!("Redis command '{other}' is not supported")));
            }
        }
        Ok(Box::new(Self { base: base_node, redis: raw.redis, command, key: raw.key }))
    }

    async fn handle(&self, msg: MsgHandle, cancel: CancellationToken) -> crate::Result<()> {
        let flow = self.flow().ok_or_else(|| N2linkError::invalid_operation("redis node has no flow"))?;
        let (settings, egress) = config_from_flow(&flow, &self.redis)?;
        let (command, key, value) = {
            let guard = msg.read().await;
            let command = guard
                .get("command")
                .and_then(Variant::as_str)
                .map(str::to_ascii_lowercase)
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| self.command.clone());
            let key = if !self.key.trim().is_empty() {
                self.key.clone()
            } else {
                guard
                    .get("key")
                    .and_then(Variant::as_str)
                    .or_else(|| guard.get("topic").and_then(Variant::as_str))
                    .unwrap_or("")
                    .to_owned()
            };
            let value = guard.get("payload").and_then(Variant::as_str).unwrap_or("").to_owned();
            (command, key, value)
        };
        tokio::select! {
            _ = cancel.cancelled() => Err(N2linkError::TaskCancelled),
            result = run_command(&settings, &egress, &command, &key, &value) => {
                let payload = result?;
                let mut guard = msg.write().await;
                guard.set("payload".to_owned(), payload);
                drop(guard);
                self.report_status(
                    StatusObject { fill: Some(StatusFill::Green), shape: Some(StatusShape::Dot), text: Some("ok".to_owned()) },
                    cancel.clone(),
                )
                .await;
                self.fan_out_one(Envelope { port: 0, msg }, cancel).await
            }
        }
    }
}

fn hide_secret(text: &str, secret: &str) -> String {
    if secret.is_empty() { text.to_owned() } else { text.replace(secret, "***") }
}

async fn run_command(
    settings: &RedisSettings,
    egress: &EgressPolicyHandle,
    command: &str,
    key: &str,
    value: &str,
) -> crate::Result<Variant> {
    let stream = egress
        .connect_tcp(EgressPurpose::Redis, NetworkProtocol::Tcp, &settings.host, settings.port)
        .await
        .map_err(|err| N2linkError::invalid_operation(&hide_secret(&err.to_string(), &settings.password)))?;
    let info = redis::RedisConnectionInfo {
        db: settings.db,
        username: None,
        password: (!settings.password.is_empty()).then(|| settings.password.clone()),
        protocol: redis::ProtocolVersion::RESP2,
    };
    #[allow(deprecated)]
    let mut connection = tokio::time::timeout(settings.timeout, Connection::new(&info, stream))
        .await
        .map_err(|_| N2linkError::Timeout)?
        .map_err(|err| N2linkError::invalid_operation(&hide_secret(&err.to_string(), &settings.password)))?;
    let map_err =
        |err: redis::RedisError| N2linkError::invalid_operation(&hide_secret(&err.to_string(), &settings.password));
    tokio::time::timeout(settings.timeout, async {
        match command {
            "ping" => {
                let _: String = redis::cmd("PING").query_async::<String>(&mut connection).await.map_err(map_err)?;
                Ok(Variant::String("PONG".to_owned()))
            }
            "get" => {
                if key.is_empty() {
                    return Err(N2linkError::invalid_operation("redis GET requires a key"));
                }
                let value: Option<String> = connection.get(key).await.map_err(map_err)?;
                Ok(value.map(Variant::String).unwrap_or(Variant::Null))
            }
            "set" => {
                if key.is_empty() {
                    return Err(N2linkError::invalid_operation("redis SET requires a key"));
                }
                let _: () = connection.set(key, value).await.map_err(map_err)?;
                Ok(Variant::String("OK".to_owned()))
            }
            "del" => {
                if key.is_empty() {
                    return Err(N2linkError::invalid_operation("redis DEL requires a key"));
                }
                let n: i64 = connection.del(key).await.map_err(map_err)?;
                Ok(Variant::Number(serde_json::Number::from(n)))
            }
            other => Err(N2linkError::NotSupported(format!("Redis command '{other}' is not supported"))),
        }
    })
    .await
    .map_err(|_| N2linkError::Timeout)?
}

#[async_trait::async_trait]
impl FlowNodeBehavior for RedisNode {
    fn get_base(&self) -> &BaseFlowNodeState {
        &self.base
    }

    async fn run(self: Arc<Self>, stop_token: CancellationToken) {
        while !stop_token.is_cancelled() {
            let cancel = stop_token.child_token();
            with_uow(self.as_ref(), cancel.child_token(), |node, msg| async move { node.handle(msg, cancel).await })
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[test]
    fn tls_is_rejected() {
        let err = resolve_settings(&json!({ "host": "127.0.0.1", "tls": true })).unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");
    }

    #[tokio::test]
    async fn a_closed_port_is_a_catchable_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "b1", "type": "redis-config", "host": "127.0.0.1", "port": port, "db": 0, "credentials": { "password": "secret-redis" } },
            { "id": "1", "z": "100", "type": "redis", "redis": "b1", "command": "ping", "wires": [["2"]] },
            { "id": "2", "z": "100", "type": "test-once" }
        ]);
        let engine = crate::runtime::engine::build_test_engine(flows).unwrap();
        let injected: Vec<(crate::runtime::model::ElementId, crate::runtime::model::Msg)> =
            Vec::deserialize(json!([["1", { "payload": 1 }]])).unwrap();
        engine.start().await.unwrap();
        let cancel = CancellationToken::new();
        engine.inject_msg(&injected[0].0, MsgHandle::new(injected[0].1.clone()), cancel).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(engine.error_count() >= 1, "connection failure was not a node error");
        engine.stop().await.unwrap();
    }
}
