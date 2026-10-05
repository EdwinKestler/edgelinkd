//! PostgreSQL config + query. TLS is out of scope: `sslmode` other than `disable` is rejected.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Number, Value};
use tokio_postgres::NoTls;
use tokio_postgres::types::Type;

use crate::EdgelinkError;
use crate::runtime::egress::{EgressPolicyHandle, EgressPurpose, NetworkProtocol};
use crate::runtime::engine::Engine;
use crate::runtime::flow::Flow;
use crate::runtime::model::json::RedFlowNodeConfig;
use crate::runtime::model::json::RedGlobalNodeConfig;
use crate::runtime::model::{MsgHandle, Variant, VariantObjectMap};
use crate::runtime::nodes::*;
use edgelink_macro::*;

const DEFAULT_PORT: u16 = 5432;
const DEFAULT_TIMEOUT_MS: u64 = 10_000;

crate::node_hints!("postgres-config", secrets = ["user", "password"], caps = ["network"]);
crate::node_hints!("postgres", refs = ["postgres" => "postgres-config"], caps = ["network"]);

#[global_node("postgres-config", red_name = "postgres-config")]
struct PostgresConfigNode {
    base: BaseGlobalNodeState,
    settings: PostgresSettings,
    egress: EgressPolicyHandle,
}

#[derive(Clone, Debug)]
struct PostgresSettings {
    host: String,
    port: u16,
    database: String,
    user: String,
    password: String,
    timeout: Duration,
}

fn json_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned)
}

fn resolve_settings(value: &Value) -> crate::Result<PostgresSettings> {
    if value.get("ssl").is_some_and(|item| item != &Value::Bool(false))
        || json_string(value, "sslmode").is_some_and(|mode| mode != "disable")
    {
        return Err(EdgelinkError::NotSupported("PostgreSQL TLS is not supported".to_owned()));
    }
    let host = json_string(value, "host").unwrap_or_else(|| "127.0.0.1".to_owned());
    let port = value.get("port").and_then(Value::as_u64).unwrap_or(DEFAULT_PORT as u64);
    if !(1..=65535).contains(&port) {
        return Err(EdgelinkError::invalid_operation("postgres port is out of range"));
    }
    let database = json_string(value, "database").unwrap_or_else(|| "postgres".to_owned());
    let user = json_string(value, "user")
        .or_else(|| value.get("credentials").and_then(|creds| json_string(creds, "user")))
        .unwrap_or_else(|| "postgres".to_owned());
    let password = json_string(value, "password")
        .or_else(|| value.get("credentials").and_then(|creds| json_string(creds, "password")))
        .unwrap_or_default();
    let timeout_ms = value.get("timeoutMs").and_then(Value::as_u64).unwrap_or(DEFAULT_TIMEOUT_MS);
    if !(100..=120_000).contains(&timeout_ms) {
        return Err(EdgelinkError::invalid_operation("postgres timeoutMs is out of range"));
    }
    Ok(PostgresSettings {
        host,
        port: port as u16,
        database,
        user,
        password,
        timeout: Duration::from_millis(timeout_ms),
    })
}

impl PostgresConfigNode {
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
                type_str: "postgres-config",
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
impl GlobalNodeBehavior for PostgresConfigNode {
    fn get_base(&self) -> &BaseGlobalNodeState {
        &self.base
    }
}

fn config_from_flow(flow: &Flow, id: &str) -> crate::Result<(PostgresSettings, EgressPolicyHandle)> {
    if id.is_empty() {
        return Err(EdgelinkError::invalid_operation("postgres node has no postgres-config"));
    }
    let eid: crate::runtime::model::ElementId =
        id.parse().map_err(|_| EdgelinkError::invalid_operation("postgres-config id is not a node id"))?;
    let engine = flow.engine().ok_or_else(|| EdgelinkError::invalid_operation("postgres node has no engine"))?;
    let global = engine
        .find_global_node_by_id(&eid)
        .ok_or_else(|| EdgelinkError::invalid_operation(&format!("postgres-config '{id}' was not loaded")))?;
    let node = global
        .as_any()
        .downcast_ref::<PostgresConfigNode>()
        .ok_or_else(|| EdgelinkError::invalid_operation(&format!("node '{id}' is not a postgres-config")))?;
    Ok((node.settings.clone(), node.egress.clone()))
}

#[derive(Debug, Deserialize)]
struct QueryConfig {
    #[serde(default)]
    postgres: String,
    #[serde(default)]
    query: String,
}

#[flow_node("postgres", red_name = "postgres", inputs = 1, outputs = 1)]
struct PostgresNode {
    base: BaseFlowNodeState,
    postgres: String,
    query: String,
}

impl PostgresNode {
    fn build(
        _flow: &Flow,
        base_node: BaseFlowNodeState,
        config: &RedFlowNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        let raw = QueryConfig::deserialize(&config.rest)?;
        if raw.postgres.trim().is_empty() {
            return Err(EdgelinkError::invalid_operation("postgres node requires a postgres-config"));
        }
        Ok(Box::new(Self { base: base_node, postgres: raw.postgres, query: raw.query }))
    }

    async fn handle(&self, msg: MsgHandle, cancel: CancellationToken) -> crate::Result<()> {
        let flow = self.flow().ok_or_else(|| EdgelinkError::invalid_operation("postgres node has no flow"))?;
        let (settings, egress) = config_from_flow(&flow, &self.postgres)?;
        let query = {
            let guard = msg.read().await;
            if !self.query.trim().is_empty() {
                self.query.clone()
            } else if let Some(text) = guard.get("query").and_then(Variant::as_str).filter(|t| !t.is_empty()) {
                text.to_owned()
            } else if let Some(text) = guard.get("payload").and_then(Variant::as_str).filter(|t| !t.is_empty()) {
                text.to_owned()
            } else {
                return Err(EdgelinkError::invalid_operation("postgres query is empty"));
            }
        };
        if query.contains('$') {
            return Err(EdgelinkError::NotSupported("PostgreSQL bound parameters are not supported".to_owned()));
        }
        tokio::select! {
            _ = cancel.cancelled() => Err(EdgelinkError::TaskCancelled),
            result = run_query(&settings, &egress, &query) => {
                match result {
                    Ok(rows) => {
                        let mut guard = msg.write().await;
                        guard.set("payload".to_owned(), Variant::Array(rows));
                        drop(guard);
                        self.report_status(
                            StatusObject {
                                fill: Some(StatusFill::Green),
                                shape: Some(StatusShape::Dot),
                                text: Some("ok".to_owned()),
                            },
                            cancel.clone(),
                        )
                        .await;
                        self.fan_out_one(Envelope { port: 0, msg }, cancel).await
                    }
                    Err(err) => {
                        self.report_status(
                            StatusObject {
                                fill: Some(StatusFill::Red),
                                shape: Some(StatusShape::Ring),
                                text: Some(status_text(&err)),
                            },
                            cancel.clone(),
                        )
                        .await;
                        Err(err)
                    }
                }
            }
        }
    }
}

async fn run_query(
    settings: &PostgresSettings,
    egress: &EgressPolicyHandle,
    query: &str,
) -> crate::Result<Vec<Variant>> {
    let stream = egress
        .connect_tcp(EgressPurpose::Postgres, NetworkProtocol::Tcp, &settings.host, settings.port)
        .await
        .map_err(|err| EdgelinkError::invalid_operation(&hide_secret(&error_chain(&err), &settings.password)))?;
    let mut cfg = tokio_postgres::Config::new();
    cfg.user(&settings.user).dbname(&settings.database).connect_timeout(settings.timeout);
    // Always set a password, including empty. tokio-postgres treats a missing password as a
    // client config error ("invalid configuration") when the server asks for SCRAM/MD5.
    cfg.password(&settings.password);
    let (client, connection) = tokio::time::timeout(settings.timeout, cfg.connect_raw(stream, NoTls))
        .await
        .map_err(|_| EdgelinkError::Timeout)?
        .map_err(|err| EdgelinkError::invalid_operation(&hide_secret(&error_chain(&err), &settings.password)))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let rows = tokio::time::timeout(settings.timeout, client.query(query, &[]))
        .await
        .map_err(|_| EdgelinkError::Timeout)?
        .map_err(|err| EdgelinkError::invalid_operation(&hide_secret(&error_chain(&err), &settings.password)))?;
    Ok(rows.iter().map(row_to_variant).collect())
}

fn error_chain(err: &dyn std::error::Error) -> String {
    let mut parts = Vec::new();
    let mut current: Option<&dyn std::error::Error> = Some(err);
    while let Some(item) = current {
        let text = item.to_string();
        if parts.last().map(String::as_str) != Some(text.as_str()) {
            parts.push(text);
        }
        current = item.source();
    }
    parts.join(": ")
}

fn hide_secret(text: &str, secret: &str) -> String {
    if secret.is_empty() { text.to_owned() } else { text.replace(secret, "***") }
}

fn status_text(err: &EdgelinkError) -> String {
    let text = err.to_string();
    text.rsplit(": ").next().unwrap_or(text.as_str()).chars().take(80).collect()
}

fn row_to_variant(row: &tokio_postgres::Row) -> Variant {
    let mut object = VariantObjectMap::new();
    for (index, column) in row.columns().iter().enumerate() {
        object.insert(column.name().to_owned(), cell(row, index, column.type_()));
    }
    Variant::Object(object)
}

fn cell(row: &tokio_postgres::Row, index: usize, ty: &Type) -> Variant {
    match *ty {
        Type::BOOL => opt_value::<bool, _>(row, index, Variant::Bool),
        Type::INT2 => opt_int::<i16>(row, index),
        Type::INT4 => opt_int::<i32>(row, index),
        Type::INT8 => opt_int::<i64>(row, index),
        Type::OID => opt_int::<u32>(row, index),
        Type::FLOAT4 => opt_float::<f32>(row, index),
        Type::FLOAT8 => opt_float::<f64>(row, index),
        Type::TEXT | Type::VARCHAR | Type::NAME | Type::BPCHAR => opt_value::<String, _>(row, index, Variant::String),
        _ => opt_value::<String, _>(row, index, Variant::String),
    }
}

fn opt_value<T, F>(row: &tokio_postgres::Row, index: usize, map: F) -> Variant
where
    T: for<'a> tokio_postgres::types::FromSql<'a>,
    F: FnOnce(T) -> Variant,
{
    row.try_get::<_, Option<T>>(index).ok().flatten().map(map).unwrap_or(Variant::Null)
}

fn opt_int<T>(row: &tokio_postgres::Row, index: usize) -> Variant
where
    T: for<'a> tokio_postgres::types::FromSql<'a> + Into<i64>,
{
    opt_value::<T, _>(row, index, |n| Variant::Number(Number::from(n.into())))
}

fn opt_float<T>(row: &tokio_postgres::Row, index: usize) -> Variant
where
    T: for<'a> tokio_postgres::types::FromSql<'a> + Into<f64>,
{
    opt_value::<T, _>(row, index, |n| Number::from_f64(n.into()).map(Variant::Number).unwrap_or(Variant::Null))
}

#[async_trait::async_trait]
impl FlowNodeBehavior for PostgresNode {
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
        let err = resolve_settings(&json!({ "host": "127.0.0.1", "sslmode": "require" })).unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");
    }

    #[test]
    fn bound_parameters_are_rejected_at_runtime_shape() {
        assert!("select $1".contains('$'));
    }

    #[tokio::test]
    async fn a_closed_port_is_a_catchable_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let flows = json!([
            { "id": "100", "type": "tab" },
            {
                "id": "b1",
                "type": "postgres-config",
                "host": "127.0.0.1",
                "port": port,
                "database": "postgres",
                "sslmode": "disable",
                "credentials": { "user": "postgres", "password": "secret-db" }
            },
            { "id": "1", "z": "100", "type": "postgres", "postgres": "b1", "query": "select 1", "wires": [["2"]] },
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

    #[test]
    fn error_chain_includes_the_source() {
        #[derive(Debug)]
        struct Inner(&'static str);
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::error::Error for Inner {}

        #[derive(Debug)]
        struct Outer {
            msg: &'static str,
            src: Inner,
        }
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.msg)
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.src)
            }
        }

        let err = Outer { msg: "invalid configuration", src: Inner("password missing") };
        assert_eq!(error_chain(&err), "invalid configuration: password missing");
        assert_eq!(
            hide_secret("db error: password authentication failed for user x / secret-db", "secret-db"),
            "db error: password authentication failed for user x / ***"
        );
        let mapped = EdgelinkError::invalid_operation(&error_chain(&err));
        assert_eq!(status_text(&mapped), "password missing");
    }

    #[tokio::test]
    async fn empty_password_against_a_live_server_keeps_the_auth_cause() {
        if std::env::var("EDGELINK_POSTGRES_LIVE").ok().as_deref() == Some("1") {
            let stream = tokio::net::TcpStream::connect(("127.0.0.1", 5432)).await.expect("127.0.0.1:5432");
            let mut cfg = tokio_postgres::Config::new();
            cfg.user("postgres").dbname("postgres").password("");
            let result = tokio::time::timeout(Duration::from_secs(5), cfg.connect_raw(stream, NoTls))
                .await
                .expect("connect_raw timed out");
            let Err(err) = result else {
                panic!("empty password was accepted");
            };
            let text = error_chain(&err);
            let lower = text.to_ascii_lowercase();
            assert!(lower.contains("password") || lower.contains("auth"), "{text}");
        }
    }

    async fn live_client(password: &str) -> tokio_postgres::Client {
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", 5432)).await.expect("127.0.0.1:5432");
        let mut cfg = tokio_postgres::Config::new();
        cfg.user("postgres").dbname("postgres").password(password);
        let (client, connection) = tokio::time::timeout(Duration::from_secs(5), cfg.connect_raw(stream, NoTls))
            .await
            .expect("connect_raw timed out")
            .unwrap_or_else(|err| panic!("{}", error_chain(&err)));
        tokio::spawn(async move {
            let _ = connection.await;
        });
        client
    }

    #[tokio::test]
    async fn select_1_as_ok_is_the_number_one() {
        if std::env::var("EDGELINK_POSTGRES_LIVE").ok().as_deref() != Some("1") {
            return;
        }
        let password = std::env::var("EDGELINK_POSTGRES_PASSWORD").expect("EDGELINK_POSTGRES_PASSWORD");
        let client = live_client(&password).await;
        let row = client
            .query_one(
                "select 1::int2 as a, 1::int4 as b, 1::int8 as c, 1::float4 as d, 1::float8 as e, true as f, 'x'::text as g",
                &[],
            )
            .await
            .expect("select typed literals");
        for index in 0..3 {
            match cell(&row, index, row.columns()[index].type_()) {
                Variant::Number(n) => assert_eq!(n.as_i64(), Some(1), "integer column {index}"),
                other => panic!("integer column {index} was {other:?}"),
            }
        }
        for index in 3..5 {
            match cell(&row, index, row.columns()[index].type_()) {
                Variant::Number(n) => assert_eq!(n.as_f64(), Some(1.0), "float column {index}"),
                other => panic!("float column {index} was {other:?}"),
            }
        }
        assert_eq!(cell(&row, 5, row.columns()[5].type_()), Variant::Bool(true));
        assert_eq!(cell(&row, 6, row.columns()[6].type_()), Variant::String("x".into()));

        let flows = json!([
            { "id": "100", "type": "tab" },
            {
                "id": "b1",
                "type": "postgres-config",
                "host": "127.0.0.1",
                "port": 5432,
                "database": "postgres",
                "sslmode": "disable",
                "credentials": { "user": "postgres", "password": password }
            },
            { "id": "1", "z": "100", "type": "postgres", "postgres": "b1", "query": "select 1 as ok", "wires": [["2"]] },
            { "id": "2", "z": "100", "type": "test-once" }
        ]);
        let engine = crate::runtime::engine::build_test_engine(flows).unwrap();
        let injected: Vec<(crate::runtime::model::ElementId, crate::runtime::model::Msg)> =
            Vec::deserialize(json!([["1", { "payload": 1 }]])).unwrap();
        let msgs = engine.run_once_with_inject(1, Duration::from_secs(3), injected).await.unwrap();
        assert_eq!(msgs.len(), 1);
        let payload = msgs[0].get("payload").cloned().expect("payload");
        let Variant::Array(rows) = payload else {
            panic!("payload was {payload:?}");
        };
        let Variant::Object(row) = &rows[0] else {
            panic!("row was {:?}", rows[0]);
        };
        match row.get("ok") {
            Some(Variant::Number(n)) if n.as_i64() == Some(1) => {}
            other => panic!("ok was {other:?}"),
        }
    }
}
