//! Modbus TCP. Coils and registers are context keys.
//!
//! A read calls [`Context::set_one`], so a forced key stays at the forced value.
//! A write calls [`Context::get_one`], so the device receives the forced value.
//! The state node reads the same keys.
//!
//! Serial RTU, other Modbus classes, and a quantity other than one are rejected.
//! This node is compiled only with the `nodes_modbus` feature.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::EdgelinkError;
use crate::runtime::context::Context;
use crate::runtime::flow::Flow;
use crate::runtime::model::json::RedFlowNodeConfig;
use crate::runtime::nodes::*;
use edgelink_macro::*;

const MIN_PERIOD_MS: u64 = 10;
const IO_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Coil,
    Discrete,
    Holding,
    Input,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    Read,
    Write,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Flow,
    Global,
    Node,
}

#[derive(Debug, Clone)]
struct Point {
    kind: Kind,
    access: Access,
    scope: Scope,
    address: u16,
    key: String,
}

struct ModbusConfig {
    host: String,
    port: u16,
    unit: u8,
    period_ms: u64,
    points: Vec<Point>,
}

#[flow_node("modbus", red_name = "modbus")]
struct ModbusNode {
    base: BaseFlowNodeState,
    config: ModbusConfig,
    link: Mutex<Option<TcpStream>>,
    tid: AtomicU16,
}

impl ModbusNode {
    fn build(
        _flow: &Flow,
        base_node: BaseFlowNodeState,
        config: &RedFlowNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        let compiled = compile(&config.rest)?;
        Ok(Box::new(ModbusNode { base: base_node, config: compiled, link: Mutex::new(None), tid: AtomicU16::new(1) }))
    }

    fn context_for(&self, scope: Scope) -> crate::Result<Context> {
        match scope {
            Scope::Node => Ok(self.get_base().context().clone()),
            Scope::Flow => self
                .flow()
                .map(|flow| flow.context().clone())
                .ok_or_else(|| EdgelinkError::invalid_operation("modbus node has no flow context")),
            Scope::Global => self
                .engine()
                .map(|engine| engine.context().clone())
                .ok_or_else(|| EdgelinkError::invalid_operation("modbus node has no global context")),
        }
    }

    async fn exchange(&self) -> crate::Result<MsgHandle> {
        let mut link = self.link.lock().await;
        if link.is_none() {
            let connect = TcpStream::connect((self.config.host.as_str(), self.config.port));
            let stream = tokio::time::timeout(IO_TIMEOUT, connect)
                .await
                .map_err(|_| EdgelinkError::invalid_operation("modbus connect timed out"))?
                .map_err(|err| EdgelinkError::invalid_operation(&format!("modbus connect failed: {err}")))?;
            *link = Some(stream);
        }
        let stream = link.as_mut().expect("stream");
        let mut values = BTreeMap::new();
        for point in &self.config.points {
            match self.one_point(stream, point).await {
                Ok(value) => {
                    values.insert(point.key.clone(), value);
                }
                Err(err) => {
                    *link = None;
                    return Err(err);
                }
            }
        }
        let mut msg = Msg::default();
        msg.set("payload".to_owned(), Variant::Object(values));
        Ok(MsgHandle::new(msg))
    }

    async fn one_point(&self, stream: &mut TcpStream, point: &Point) -> crate::Result<Variant> {
        let ctx = self.context_for(point.scope)?;
        match point.access {
            Access::Read => {
                let value = read_point(stream, &self.tid, self.config.unit, point).await?;
                ctx.set_one(None, &point.key, Some(value.clone()), &[]).await?;
                Ok(ctx.get_one(None, &point.key, &[]).await.unwrap_or(value))
            }
            Access::Write => {
                let Some(value) = ctx.get_one(None, &point.key, &[]).await else {
                    return Ok(Variant::Null);
                };
                write_point(stream, &self.tid, self.config.unit, point, &value).await?;
                Ok(value)
            }
            Access::ReadWrite => {
                let device = read_point(stream, &self.tid, self.config.unit, point).await?;
                ctx.set_one(None, &point.key, Some(device.clone()), &[]).await?;
                let stored = ctx.get_one(None, &point.key, &[]).await.unwrap_or(device);
                write_point(stream, &self.tid, self.config.unit, point, &stored).await?;
                Ok(stored)
            }
        }
    }

    async fn emit(&self, cancel: CancellationToken) -> crate::Result<()> {
        match self.exchange().await {
            Ok(msg) => {
                self.report_status(status_ok(), cancel.clone()).await;
                self.fan_out_one(Envelope { port: 0, msg }, cancel).await
            }
            Err(err) => {
                log::warn!("Modbus exchange failed: {err}");
                self.report_status(status_bad(&err.to_string()), cancel).await;
                Err(err)
            }
        }
    }
}

fn status_ok() -> StatusObject {
    StatusObject { fill: Some(StatusFill::Green), shape: Some(StatusShape::Dot), text: Some("ok".to_owned()) }
}

fn status_bad(text: &str) -> StatusObject {
    StatusObject { fill: Some(StatusFill::Red), shape: Some(StatusShape::Dot), text: Some(text.to_owned()) }
}

#[async_trait::async_trait]
impl FlowNodeBehavior for ModbusNode {
    fn get_base(&self) -> &BaseFlowNodeState {
        &self.base
    }

    async fn run(self: Arc<Self>, stop_token: CancellationToken) {
        if self.config.period_ms > 0 {
            let node = self.clone();
            let cancel = stop_token.clone();
            let period = Duration::from_millis(self.config.period_ms);
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep(period) => {
                            if let Err(err) = node.emit(cancel.clone()).await {
                                node.report_error(err.to_string(), MsgHandle::new(Msg::default()), cancel.clone()).await;
                            }
                        }
                    }
                }
            });
        }
        while !stop_token.is_cancelled() {
            let node = self.clone();
            let cancel = stop_token.clone();
            with_uow(node.as_ref(), cancel.clone(), |node, _msg| async move { node.emit(cancel).await }).await;
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default = "default_unit")]
    unit: u8,
    #[serde(default)]
    period: u64,
    #[serde(default = "tcp_transport")]
    transport: String,
    #[serde(default)]
    points: Vec<RawPoint>,
}

#[derive(Debug, Deserialize)]
struct RawPoint {
    kind: String,
    address: u16,
    key: String,
    #[serde(default = "flow_scope")]
    scope: String,
    #[serde(default = "read_access")]
    access: String,
    #[serde(default)]
    quantity: Option<u16>,
    #[serde(default)]
    function: Option<u8>,
}

fn default_port() -> u16 {
    502
}

fn default_unit() -> u8 {
    1
}

fn tcp_transport() -> String {
    "tcp".to_owned()
}

fn flow_scope() -> String {
    "flow".to_owned()
}

fn read_access() -> String {
    "read".to_owned()
}

fn compile(value: &serde_json::Value) -> crate::Result<ModbusConfig> {
    if value.get("serial").is_some() || value.get("baudrate").is_some() || value.get("device").is_some() {
        return Err(EdgelinkError::NotSupported("Modbus serial RTU is not supported".to_owned()));
    }
    let raw = RawConfig::deserialize(value)?;
    if raw.transport != "tcp" {
        return Err(EdgelinkError::NotSupported(format!("Modbus {} is not supported", raw.transport)));
    }
    if raw.host.trim().is_empty() {
        return Err(EdgelinkError::invalid_operation("modbus host is empty"));
    }
    if raw.period > 0 && raw.period < MIN_PERIOD_MS {
        return Err(EdgelinkError::invalid_operation(&format!(
            "modbus period {0} is below the {MIN_PERIOD_MS} ms scheduling floor",
            raw.period
        )));
    }
    if raw.points.is_empty() {
        return Err(EdgelinkError::invalid_operation("modbus node has no points"));
    }
    let mut points = Vec::with_capacity(raw.points.len());
    let mut seen = BTreeMap::new();
    for point in raw.points {
        let compiled = compile_point(point)?;
        if seen.insert(compiled.key.clone(), ()).is_some() {
            return Err(EdgelinkError::invalid_operation(&format!("modbus key '{}' is duplicated", compiled.key)));
        }
        points.push(compiled);
    }
    Ok(ModbusConfig { host: raw.host, port: raw.port, unit: raw.unit, period_ms: raw.period, points })
}

fn compile_point(raw: RawPoint) -> crate::Result<Point> {
    if raw.quantity.is_some_and(|quantity| quantity != 1) {
        return Err(EdgelinkError::NotSupported("Modbus quantity other than 1 is not supported".to_owned()));
    }
    let kind = match raw.kind.as_str() {
        "coil" => Kind::Coil,
        "discrete" => Kind::Discrete,
        "holding" => Kind::Holding,
        "input" => Kind::Input,
        other => return Err(EdgelinkError::NotSupported(format!("Modbus point kind '{other}' is not supported"))),
    };
    let access = match raw.access.as_str() {
        "read" => Access::Read,
        "write" => Access::Write,
        "readwrite" => Access::ReadWrite,
        other => return Err(EdgelinkError::NotSupported(format!("Modbus access '{other}' is not supported"))),
    };
    if matches!(kind, Kind::Discrete | Kind::Input) && matches!(access, Access::Write | Access::ReadWrite) {
        return Err(EdgelinkError::NotSupported(format!("writing a Modbus {} is not supported", raw.kind)));
    }
    if let Some(function) = raw.function {
        let allowed: &[u8] = match (kind, access) {
            (Kind::Coil, Access::Read) => &[1],
            (Kind::Coil, Access::Write) => &[5],
            (Kind::Coil, Access::ReadWrite) => &[1, 5],
            (Kind::Discrete, Access::Read) => &[2],
            (Kind::Holding, Access::Read) => &[3],
            (Kind::Holding, Access::Write) => &[6],
            (Kind::Holding, Access::ReadWrite) => &[3, 6],
            (Kind::Input, Access::Read) => &[4],
            _ => &[],
        };
        if !allowed.contains(&function) {
            return Err(EdgelinkError::NotSupported(format!("Modbus function {function} is not supported")));
        }
    }
    let scope = match raw.scope.as_str() {
        "flow" => Scope::Flow,
        "global" => Scope::Global,
        "node" => Scope::Node,
        other => return Err(EdgelinkError::NotSupported(format!("Modbus context scope '{other}' is not supported"))),
    };
    if raw.key.trim().is_empty() {
        return Err(EdgelinkError::invalid_operation("modbus point key is empty"));
    }
    Ok(Point { kind, access, scope, address: raw.address, key: raw.key })
}

async fn read_point(stream: &mut TcpStream, tid: &AtomicU16, unit: u8, point: &Point) -> crate::Result<Variant> {
    let function = match point.kind {
        Kind::Coil => 1,
        Kind::Discrete => 2,
        Kind::Holding => 3,
        Kind::Input => 4,
    };
    let pdu = read_pdu(function, point.address);
    let response = transact(stream, tid, unit, &pdu).await?;
    decode_read(point.kind, &response)
}

async fn write_point(
    stream: &mut TcpStream,
    tid: &AtomicU16,
    unit: u8,
    point: &Point,
    value: &Variant,
) -> crate::Result<()> {
    let pdu = match point.kind {
        Kind::Coil => {
            let on = match value {
                Variant::Bool(flag) => *flag,
                Variant::Number(number) => number.as_u64().unwrap_or(0) != 0,
                _ => {
                    return Err(EdgelinkError::invalid_operation(&format!(
                        "modbus coil '{}' is not a boolean",
                        point.key
                    )));
                }
            };
            let bits: u16 = if on { 0xff00 } else { 0 };
            write_pdu(5, point.address, bits)
        }
        Kind::Holding => {
            let number = value.as_u64().ok_or_else(|| {
                EdgelinkError::invalid_operation(&format!("modbus register '{}' is not a number", point.key))
            })?;
            let register = u16::try_from(number).map_err(|_| {
                EdgelinkError::invalid_operation(&format!("modbus register '{}' does not fit in 16 bits", point.key))
            })?;
            write_pdu(6, point.address, register)
        }
        Kind::Discrete | Kind::Input => {
            return Err(EdgelinkError::NotSupported(format!("writing a Modbus {} is not supported", point.key)));
        }
    };
    let _ = transact(stream, tid, unit, &pdu).await?;
    Ok(())
}

fn read_pdu(function: u8, address: u16) -> [u8; 5] {
    let addr = address.to_be_bytes();
    [function, addr[0], addr[1], 0, 1]
}

fn write_pdu(function: u8, address: u16, value: u16) -> [u8; 5] {
    let addr = address.to_be_bytes();
    let data = value.to_be_bytes();
    [function, addr[0], addr[1], data[0], data[1]]
}

struct Mbap {
    tid: u16,
    protocol: u16,
    unit: u8,
    pdu: Vec<u8>,
}

async fn transact(stream: &mut TcpStream, tid: &AtomicU16, unit: u8, pdu: &[u8]) -> crate::Result<Vec<u8>> {
    let id = tid.fetch_add(1, Ordering::Relaxed);
    let request = frame(id, unit, pdu);
    let write = stream.write_all(&request);
    tokio::time::timeout(IO_TIMEOUT, write)
        .await
        .map_err(|_| EdgelinkError::invalid_operation("modbus write timed out"))?
        .map_err(|err| EdgelinkError::invalid_operation(&format!("modbus write failed: {err}")))?;
    let response = tokio::time::timeout(IO_TIMEOUT, read_mbap(stream))
        .await
        .map_err(|_| EdgelinkError::invalid_operation("modbus read timed out"))?
        .map_err(|err| EdgelinkError::invalid_operation(&format!("modbus read failed: {err}")))?;
    check_response(id, unit, pdu, &response)?;
    Ok(response.pdu)
}

fn frame(tid: u16, unit: u8, pdu: &[u8]) -> Vec<u8> {
    let body_len = u16::try_from(pdu.len().saturating_add(1)).unwrap_or(u16::MAX);
    let mut out = Vec::with_capacity(7 + pdu.len());
    out.extend(tid.to_be_bytes());
    out.extend(0u16.to_be_bytes());
    out.extend(body_len.to_be_bytes());
    out.push(unit);
    out.extend(pdu);
    out
}

async fn read_mbap(stream: &mut TcpStream) -> std::io::Result<Mbap> {
    let mut header = [0u8; 6];
    stream.read_exact(&mut header).await?;
    let tid = u16::from_be_bytes([header[0], header[1]]);
    let protocol = u16::from_be_bytes([header[2], header[3]]);
    let len = usize::from(u16::from_be_bytes([header[4], header[5]]));
    if len == 0 || len > 260 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "modbus frame length"));
    }
    let mut rest = vec![0u8; len];
    stream.read_exact(&mut rest).await?;
    if rest.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "modbus frame"));
    }
    Ok(Mbap { tid, protocol, unit: rest[0], pdu: rest[1..].to_vec() })
}

fn check_response(tid: u16, unit: u8, request: &[u8], response: &Mbap) -> crate::Result<()> {
    if response.tid != tid {
        return Err(EdgelinkError::invalid_operation("modbus transaction id mismatch"));
    }
    if response.protocol != 0 {
        return Err(EdgelinkError::invalid_operation("modbus protocol id is not zero"));
    }
    if response.unit != unit {
        return Err(EdgelinkError::invalid_operation("modbus unit id mismatch"));
    }
    if request.is_empty() || response.pdu.is_empty() {
        return Err(EdgelinkError::invalid_operation("modbus response is short"));
    }
    let function = request[0];
    let reply = response.pdu[0];
    if reply == function | 0x80 {
        if response.pdu.len() != 2 {
            return Err(EdgelinkError::invalid_operation("modbus exception length"));
        }
        let code = response.pdu[1];
        return Err(EdgelinkError::invalid_operation(&format!("modbus exception {code}")));
    }
    if reply != function {
        return Err(EdgelinkError::invalid_operation("modbus function code mismatch"));
    }
    match function {
        1 | 2 => {
            let byte_count =
                *response.pdu.get(1).ok_or_else(|| EdgelinkError::invalid_operation("modbus response is short"))?;
            if usize::from(byte_count) != 1 || response.pdu.len() != 3 {
                return Err(EdgelinkError::invalid_operation("modbus bit byte count mismatch"));
            }
        }
        3 | 4 => {
            let byte_count =
                *response.pdu.get(1).ok_or_else(|| EdgelinkError::invalid_operation("modbus response is short"))?;
            if usize::from(byte_count) != 2 || response.pdu.len() != 4 {
                return Err(EdgelinkError::invalid_operation("modbus register byte count mismatch"));
            }
        }
        5 | 6 => {
            if response.pdu.as_slice() != request {
                return Err(EdgelinkError::invalid_operation("modbus write echo mismatch"));
            }
        }
        _ => return Err(EdgelinkError::NotSupported(format!("Modbus function {function} is not supported"))),
    }
    Ok(())
}

fn decode_read(kind: Kind, pdu: &[u8]) -> crate::Result<Variant> {
    match kind {
        Kind::Coil | Kind::Discrete => {
            let value = *pdu.get(2).ok_or_else(|| EdgelinkError::invalid_operation("modbus response is short"))?;
            Ok(Variant::from(value & 1 == 1))
        }
        Kind::Holding | Kind::Input => {
            if pdu.len() < 4 {
                return Err(EdgelinkError::invalid_operation("modbus register response is short"));
            }
            let value = u16::from_be_bytes([pdu[2], pdu[3]]);
            Ok(Variant::from(u64::from(value)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    fn engine_with(port: u16, points: serde_json::Value) -> crate::Result<crate::runtime::engine::Engine> {
        let flows = json!([
            { "id": "100", "type": "tab" },
            {
                "id": "1",
                "z": "100",
                "type": "modbus",
                "host": "127.0.0.1",
                "port": port,
                "unit": 1,
                "period": 0,
                "transport": "tcp",
                "points": points,
                "wires": [["2"]]
            },
            { "id": "2", "z": "100", "type": "test-once" }
        ]);
        crate::runtime::engine::build_test_engine(flows)
    }

    async fn serve_coil_and_register(listener: tokio::net::TcpListener, written: Arc<AtomicU16>) {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        loop {
            let Ok(request) = read_mbap(&mut stream).await else {
                return;
            };
            if request.pdu.is_empty() {
                return;
            }
            let response = match request.pdu[0] {
                1 | 2 => vec![request.pdu[0], 1, 0x01],
                3 | 4 => vec![request.pdu[0], 2, 0x00, 0x29],
                6 => {
                    if request.pdu.len() >= 5 {
                        written.store(u16::from_be_bytes([request.pdu[3], request.pdu[4]]), Ordering::Relaxed);
                    }
                    request.pdu.clone()
                }
                5 => request.pdu.clone(),
                _ => vec![request.pdu[0] | 0x80, 0x01],
            };
            if stream.write_all(&frame(request.tid, request.unit, &response)).await.is_err() {
                return;
            }
        }
    }

    #[tokio::test]
    async fn a_forced_coil_is_not_overwritten_by_a_read() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let written = Arc::new(AtomicU16::new(0));
        tokio::spawn(serve_coil_and_register(listener, written));
        let engine = engine_with(
            port,
            json!([{ "kind": "coil", "address": 0, "key": "pump", "scope": "flow", "access": "read" }]),
        )
        .unwrap();
        let flow = engine.get_flow(&"100".parse().unwrap()).unwrap();
        flow.context().force_one(None, "pump", Variant::from(false)).unwrap();
        let injected: Vec<(ElementId, Msg)> = Vec::deserialize(json!([["1", { "payload": 1 }]])).unwrap();
        let msgs = engine.run_once_with_inject(1, Duration::from_secs(2), injected).await.unwrap();
        let payload = msgs[0].get("payload").unwrap();
        assert_eq!(payload.as_object().unwrap().get("pump").cloned(), Some(Variant::from(false)));
        assert_eq!(flow.context().get_one(None, "pump", &[]).await, Some(Variant::from(false)));
    }

    #[tokio::test]
    async fn a_write_sends_the_forced_register() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let written = Arc::new(AtomicU16::new(0xffff));
        tokio::spawn(serve_coil_and_register(listener, written.clone()));
        let engine = engine_with(
            port,
            json!([{ "kind": "holding", "address": 10, "key": "setpoint", "scope": "flow", "access": "write" }]),
        )
        .unwrap();
        let flow = engine.get_flow(&"100".parse().unwrap()).unwrap();
        flow.context().force_one(None, "setpoint", Variant::from(7u64)).unwrap();
        let injected: Vec<(ElementId, Msg)> = Vec::deserialize(json!([["1", { "payload": 1 }]])).unwrap();
        let _msgs = engine.run_once_with_inject(1, Duration::from_secs(2), injected).await.unwrap();
        assert_eq!(written.load(Ordering::Relaxed), 7);
    }

    #[test]
    fn serial_rtu_and_other_classes_are_rejected() {
        let serial = json!([
            { "id": "100", "type": "tab" },
            {
                "id": "1", "z": "100", "type": "modbus", "host": "127.0.0.1", "transport": "rtu",
                "points": [{ "kind": "coil", "address": 0, "key": "pump" }],
                "wires": []
            }
        ]);
        let err = crate::runtime::engine::build_test_engine(serial).unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");

        let device = json!([
            { "id": "100", "type": "tab" },
            {
                "id": "1", "z": "100", "type": "modbus", "host": "127.0.0.1", "device": "/dev/ttyUSB0",
                "points": [{ "kind": "coil", "address": 0, "key": "pump" }],
                "wires": []
            }
        ]);
        let err = crate::runtime::engine::build_test_engine(device).unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");
    }

    fn mbap(tid: u16, protocol: u16, unit: u8, pdu: Vec<u8>) -> Mbap {
        Mbap { tid, protocol, unit, pdu }
    }

    #[test]
    fn responses_that_do_not_match_the_request_are_rejected() {
        let read_bits = [1u8, 0, 0, 0, 1];
        let read_regs = [3u8, 0, 0, 0, 1];
        let write_reg = [6u8, 0, 10, 0, 7];
        assert!(
            check_response(1, 1, &read_bits, &mbap(2, 0, 1, vec![1, 1, 1]))
                .unwrap_err()
                .to_string()
                .contains("transaction id")
        );
        assert!(
            check_response(1, 1, &read_bits, &mbap(1, 1, 1, vec![1, 1, 1]))
                .unwrap_err()
                .to_string()
                .contains("protocol id")
        );
        assert!(
            check_response(1, 1, &read_bits, &mbap(1, 0, 2, vec![1, 1, 1]))
                .unwrap_err()
                .to_string()
                .contains("unit id")
        );
        assert!(
            check_response(1, 1, &read_bits, &mbap(1, 0, 1, vec![3, 1, 1]))
                .unwrap_err()
                .to_string()
                .contains("function code")
        );
        assert!(
            check_response(1, 1, &read_bits, &mbap(1, 0, 1, vec![1, 2, 1, 0]))
                .unwrap_err()
                .to_string()
                .contains("byte count")
        );
        assert!(
            check_response(1, 1, &read_regs, &mbap(1, 0, 1, vec![3, 1, 0]))
                .unwrap_err()
                .to_string()
                .contains("byte count")
        );
        assert!(
            check_response(1, 1, &write_reg, &mbap(1, 0, 1, vec![6, 0, 10, 0, 8]))
                .unwrap_err()
                .to_string()
                .contains("echo")
        );
        let err = check_response(1, 1, &read_bits, &mbap(1, 0, 1, vec![0x81, 0x02])).unwrap_err();
        assert!(err.to_string().contains("exception 2"), "{err}");
        let extra = check_response(1, 1, &read_bits, &mbap(1, 0, 1, vec![0x81, 0x02, 0x00])).unwrap_err();
        assert!(extra.to_string().contains("exception length"), "{extra}");
        check_response(1, 1, &read_bits, &mbap(1, 0, 1, vec![1, 1, 1])).unwrap();
        check_response(1, 1, &read_regs, &mbap(1, 0, 1, vec![3, 2, 0, 0x29])).unwrap();
        check_response(1, 1, &write_reg, &mbap(1, 0, 1, write_reg.to_vec())).unwrap();
    }

    #[tokio::test]
    async fn transact_rejects_a_wrong_transaction_id() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(request) = read_mbap(&mut stream).await else {
                return;
            };
            let _ = stream.write_all(&frame(request.tid.wrapping_add(1), request.unit, &[1, 1, 1])).await;
        });
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let tid = AtomicU16::new(1);
        let err = transact(&mut stream, &tid, 1, &[1, 0, 0, 0, 1]).await.unwrap_err();
        assert!(err.to_string().contains("transaction id"), "{err}");
    }

    #[tokio::test]
    async fn an_exception_is_catchable_and_the_next_exchange_reconnects() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let exchanges = Arc::new(AtomicU16::new(0));
        let seen = exchanges.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let Ok(request) = read_mbap(&mut stream).await else {
                    continue;
                };
                let n = seen.fetch_add(1, Ordering::AcqRel);
                let pdu: &[u8] = if n == 0 { &[0x81, 0x02] } else { &[1, 1, 1] };
                let _ = stream.write_all(&frame(request.tid, request.unit, pdu)).await;
            }
        });
        let engine = crate::runtime::engine::build_test_engine(json!([
            { "id": "100", "type": "tab" },
            {
                "id": "1",
                "z": "100",
                "type": "modbus",
                "host": "127.0.0.1",
                "port": port,
                "unit": 1,
                "period": 0,
                "transport": "tcp",
                "points": [{ "kind": "coil", "address": 0, "key": "pump", "scope": "flow", "access": "read" }],
                "wires": [["2"]]
            },
            { "id": "2", "z": "100", "type": "test-once" },
            { "id": "3", "z": "100", "type": "catch", "scope": ["1"], "uncaught": false, "wires": [["2"]] }
        ]))
        .unwrap();
        let injected = Vec::<(ElementId, Msg, f64)>::deserialize(json!([
            ["1", { "payload": 1 }, 0.0],
            ["1", { "payload": 1 }, 400.0]
        ]))
        .unwrap();
        let msgs = engine.run_window_with_schedule(Duration::from_secs(2), injected).await.unwrap();
        let caught: Vec<_> = msgs
            .iter()
            .filter(|(msg, _, _)| {
                msg.get_nav("error.message")
                    .and_then(|value| value.as_str())
                    .is_some_and(|text| text.contains("exception 2"))
            })
            .collect();
        assert_eq!(caught.len(), 1, "expected one catch event: {msgs:?}");
        assert_eq!(engine.error_count(), 1, "expected one node error, got {}", engine.error_count());
        let ok = msgs.iter().find(|(msg, _, _)| {
            msg.get("payload").and_then(|value| value.as_object()).and_then(|obj| obj.get("pump")).is_some()
        });
        let payload = ok.expect("reconnected exchange produced no payload").0.get("payload").unwrap();
        assert_eq!(payload.as_object().unwrap().get("pump").cloned(), Some(Variant::from(true)));
    }
}
