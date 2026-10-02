//! One soft real-time scan for the whole process.
//!
//! The task waits `runtime.scan.period_ms`, measures that body, and writes the same `scan`
//! record into every flow context. It does not walk wires. `overrun` on a record is whether
//! the previous body exceeded the period; a later body that fits clears the flag on the
//! following scan. A `scan` node is only the status lamp (green `ok`, red `overrun`).
//!
//! The host clock is `Instant` around the status update. Tests pass a scripted duration
//! through the same cursor and do not sleep. [`MIN_SCAN_PERIOD_MS`] is a scheduling floor,
//! not a bound on wake latency.

use std::time::Duration;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::EdgelinkError;
use crate::runtime::engine::Engine;
use crate::runtime::model::{ContextHolder, FlowsElement, Variant};
use crate::runtime::nodes::{StatusFill, StatusObject, StatusShape};

/// Shortest period the runtime will schedule, in milliseconds.
///
/// A configured period below this is rejected at start. The constant is a floor on the
/// timer the process is willing to arm, not a guarantee that the host wakes in time.
pub const MIN_SCAN_PERIOD_MS: u64 = 10;

pub const SCAN_CONTEXT_KEY: &str = "scan";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanSample {
    pub seq: u64,
    pub period_ms: u64,
    pub duration_ms: u64,
    pub overrun: bool,
}

impl ScanSample {
    fn to_variant(&self) -> Variant {
        Variant::from([
            ("seq", Variant::from(self.seq)),
            ("period", Variant::from(self.period_ms)),
            ("duration", Variant::from(self.duration_ms)),
            ("overrun", Variant::from(self.overrun)),
        ])
    }
}

/// `duration_ms` is this body's measured time. The sample's `overrun` is `previous_overrun`.
/// The bool is whether this body exceeded `period_ms`, which the next sample reports.
pub fn assess(seq: u64, period_ms: u64, duration_ms: u64, previous_overrun: bool) -> (ScanSample, bool) {
    let this_overran = duration_ms > period_ms;
    (ScanSample { seq, period_ms, duration_ms, overrun: previous_overrun }, this_overran)
}

#[derive(Debug)]
pub struct ScanCursor {
    next_seq: u64,
    previous_overrun: bool,
}

impl ScanCursor {
    pub fn new() -> Self {
        Self { next_seq: 1, previous_overrun: false }
    }

    pub fn overrun(&self) -> bool {
        self.previous_overrun
    }

    pub fn push(&mut self, period_ms: u64, duration_ms: u64) -> ScanSample {
        let (sample, this_overran) = assess(self.next_seq, period_ms, duration_ms, self.previous_overrun);
        self.previous_overrun = this_overran;
        self.next_seq += 1;
        sample
    }
}

impl Default for ScanCursor {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct ScanSettings {
    #[serde(default)]
    period_ms: u64,
}

/// `0` when the task stays off. A period in `1..MIN_SCAN_PERIOD_MS` is an error.
pub fn configured_period(cfg: Option<&config::Config>) -> crate::Result<u64> {
    let period_ms = match cfg {
        Some(cfg) => match cfg.get::<ScanSettings>("runtime.scan") {
            Ok(settings) => settings.period_ms,
            Err(config::ConfigError::NotFound(_)) => 0,
            Err(err) => return Err(err.into()),
        },
        None => 0,
    };
    if period_ms == 0 {
        return Ok(0);
    }
    if period_ms < MIN_SCAN_PERIOD_MS {
        return Err(EdgelinkError::InvalidOperation(format!(
            "runtime.scan.period_ms {period_ms} is below the {MIN_SCAN_PERIOD_MS} ms scheduling floor"
        )));
    }
    Ok(period_ms)
}

pub async fn run(engine: Engine, period_ms: u64, cancel: CancellationToken) {
    let mut cursor = ScanCursor::new();
    let period = Duration::from_millis(period_ms);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(period) => {}
        }
        if cancel.is_cancelled() {
            break;
        }
        // The body is the status update. The context write publishes the measurement.
        let started = std::time::Instant::now();
        paint(&engine, cursor.overrun(), &cancel).await;
        let duration_ms = elapsed_ms(started);
        if let Err(err) = commit(&engine, &mut cursor, period_ms, duration_ms).await {
            log::error!("Scan task failed to write flow.scan: {err}");
        }
    }
}

/// One scripted step: paint the previous overrun, then store this body's duration.
#[cfg(test)]
pub async fn record(
    engine: &Engine,
    cursor: &mut ScanCursor,
    period_ms: u64,
    duration_ms: u64,
    cancel: &CancellationToken,
) -> crate::Result<()> {
    paint(engine, cursor.overrun(), cancel).await;
    commit(engine, cursor, period_ms, duration_ms).await
}

async fn commit(engine: &Engine, cursor: &mut ScanCursor, period_ms: u64, duration_ms: u64) -> crate::Result<()> {
    let sample = cursor.push(period_ms, duration_ms);
    write_context(engine, &sample).await
}

fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn status_for(overrun: bool) -> StatusObject {
    if overrun {
        StatusObject { fill: Some(StatusFill::Red), shape: Some(StatusShape::Dot), text: Some("overrun".to_owned()) }
    } else {
        StatusObject { fill: Some(StatusFill::Green), shape: Some(StatusShape::Dot), text: Some("ok".to_owned()) }
    }
}

async fn paint(engine: &Engine, overrun: bool, cancel: &CancellationToken) {
    let status = status_for(overrun);
    for flow in engine.flows_snapshot() {
        for node in flow.get_all_flow_nodes() {
            if node.type_str() == "scan" && !node.is_disabled() {
                node.report_status(status.clone(), cancel.child_token()).await;
            }
        }
    }
}

async fn write_context(engine: &Engine, sample: &ScanSample) -> crate::Result<()> {
    let value = sample.to_variant();
    let mut first_err = None;
    for flow in engine.flows_snapshot() {
        if let Err(err) = flow.context().set_one(None, SCAN_CONTEXT_KEY, Some(value.clone()), &[]).await {
            log::error!("Scan task failed to write flow.scan on '{}': {err}", flow.id());
            if first_err.is_none() {
                first_err = Some(err);
            }
        }
    }
    match first_err {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::nodes::StatusFill;
    use serde_json::json;

    #[test]
    fn stub_clock_sets_overrun_on_the_next_scan_and_clears_it() {
        let (first, first_overran) = assess(1, 10, 30, false);
        assert_eq!(first, ScanSample { seq: 1, period_ms: 10, duration_ms: 30, overrun: false });
        assert!(first_overran);

        let (over, over_overran) = assess(2, 10, 4, first_overran);
        assert_eq!(over, ScanSample { seq: 2, period_ms: 10, duration_ms: 4, overrun: true });
        assert!(!over_overran);

        let (quiet, quiet_overran) = assess(3, 10, 4, over_overran);
        assert_eq!(quiet, ScanSample { seq: 3, period_ms: 10, duration_ms: 4, overrun: false });
        assert!(!quiet_overran);

        let mut cursor = ScanCursor::new();
        assert!(!cursor.push(10, 10).overrun);
        assert!(!cursor.push(10, 1).overrun);
    }

    #[test]
    fn period_zero_is_off_and_a_short_period_is_an_error() {
        assert_eq!(MIN_SCAN_PERIOD_MS, 10);
        assert_eq!(configured_period(None).unwrap(), 0);

        let off = config_with_period(0);
        assert_eq!(configured_period(Some(&off)).unwrap(), 0);

        let held = config_with_period(10);
        assert_eq!(configured_period(Some(&held)).unwrap(), MIN_SCAN_PERIOD_MS);

        let bad = config_with_period(9);
        let err = configured_period(Some(&bad)).unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, EdgelinkError::InvalidOperation(_)), "{text}");
        assert!(text.contains("9"), "{text}");
        assert!(text.contains("10"), "{text}");
        assert!(text.contains("scheduling floor"), "{text}");
    }

    #[tokio::test]
    async fn scripted_durations_write_every_flow_and_paint_every_scan_node() {
        let engine = crate::runtime::engine::build_test_engine(json!([
            { "id": "100", "type": "tab", "label": "Flow 1" },
            { "id": "200", "type": "tab", "label": "Flow 2" },
            { "id": "1", "z": "100", "type": "scan", "name": "lamp-a" },
            { "id": "3", "z": "100", "type": "scan", "name": "lamp-b" }
        ]))
        .unwrap();
        let mut status = engine.status_channel().subscribe();
        let mut cursor = ScanCursor::new();
        let cancel = CancellationToken::new();
        record(&engine, &mut cursor, 10, 30, &cancel).await.unwrap();
        record(&engine, &mut cursor, 10, 4, &cancel).await.unwrap();
        record(&engine, &mut cursor, 10, 4, &cancel).await.unwrap();

        let expected = ScanSample { seq: 3, period_ms: 10, duration_ms: 4, overrun: false };
        assert_eq!(read_sample(&engine, "100").await, expected);
        assert_eq!(read_sample(&engine, "200").await, expected);

        let mut fills = Vec::new();
        while let Ok(message) = status.try_recv() {
            fills.push(message.status.fill);
        }
        assert_eq!(
            fills,
            vec![
                Some(StatusFill::Green),
                Some(StatusFill::Green),
                Some(StatusFill::Red),
                Some(StatusFill::Red),
                Some(StatusFill::Green),
                Some(StatusFill::Green),
            ]
        );
    }

    #[tokio::test]
    async fn a_forced_scan_key_is_left_in_place() {
        let engine = crate::runtime::engine::build_test_engine(json!([
            { "id": "100", "type": "tab", "label": "Flow 1" },
            { "id": "1", "z": "100", "type": "scan" }
        ]))
        .unwrap();
        let flow = engine.get_flow(&"100".parse().unwrap()).unwrap();
        let ctx = flow.context().clone();
        ctx.force_one(None, SCAN_CONTEXT_KEY, Variant::from("held")).unwrap();

        let mut cursor = ScanCursor::new();
        record(&engine, &mut cursor, 10, 30, &CancellationToken::new()).await.unwrap();
        assert_eq!(ctx.get_one(None, SCAN_CONTEXT_KEY, &[]).await.unwrap(), Variant::from("held"));
    }

    #[tokio::test]
    async fn engine_start_rejects_a_period_below_the_floor() {
        let engine = engine_with_period(9);
        let err = engine.start().await.unwrap_err();
        let text = err.to_string();
        assert!(text.contains("scheduling floor"), "{text}");
        assert!(text.contains('9'), "{text}");
    }

    #[tokio::test]
    async fn a_zero_period_does_not_start_the_task() {
        let engine = engine_with_period(0);
        engine.start().await.unwrap();
        engine.stop().await.unwrap();
        let flow = engine.get_flow(&"100".parse().unwrap()).unwrap();
        assert!(flow.context().get_one(None, SCAN_CONTEXT_KEY, &[]).await.is_none());
    }

    fn config_with_period(period_ms: i64) -> config::Config {
        config::Config::builder().set_override("runtime.scan.period_ms", period_ms).unwrap().build().unwrap()
    }

    fn engine_with_period(period_ms: i64) -> Engine {
        let registry = crate::runtime::registry::RegistryBuilder::default().build().unwrap();
        let elcfg = config::Config::builder()
            .set_override("runtime.context.default", "memory")
            .unwrap()
            .set_override("runtime.context.stores.memory.provider", "memory")
            .unwrap()
            .set_override("runtime.scan.period_ms", period_ms)
            .unwrap()
            .build()
            .unwrap();
        Engine::with_json(&registry, json!([{ "id": "100", "type": "tab", "label": "Flow 1" }]), Some(elcfg)).unwrap()
    }

    async fn read_sample(engine: &Engine, flow_id: &str) -> ScanSample {
        let flow = engine.get_flow(&flow_id.parse().unwrap()).unwrap();
        let value = flow.context().get_one(None, SCAN_CONTEXT_KEY, &[]).await.unwrap();
        let obj = value.as_object().unwrap();
        ScanSample {
            seq: obj.get("seq").and_then(Variant::as_u64).unwrap(),
            period_ms: obj.get("period").and_then(Variant::as_u64).unwrap(),
            duration_ms: obj.get("duration").and_then(Variant::as_u64).unwrap(),
            overrun: obj.get("overrun").and_then(Variant::as_bool).unwrap(),
        }
    }
}
