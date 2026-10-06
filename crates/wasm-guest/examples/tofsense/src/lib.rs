//! Example plugin: Nooploop TOFSense laser ranging sensor over UART (user manual V2.5, §8).
//!
//! A WASM plugin cannot open the serial port (no I/O by design), so the UART reaches the flow
//! through a built-in node, for example the port exposed over TCP with `ser2net`/`socat` and read
//! by `tcp in`. This plugin is the protocol part: it reassembles the byte stream into
//! `NLink_TOFSense_Frame0` frames, checks them and turns them into readings, and builds
//! `NLink_TOFSense_Read_Frame0` query frames for UART query (cascade) mode.

use n2link_wasm_guest::{export_node, manifest, Ctx, EveValue, Fill, Msg, Node, Shape};

manifest!("../plugin.toml");

/// `NLink_TOFSense_Frame0`: 0x57 0x00, reserved, id, system_time u32, dis×1000 int24,
/// dis_status, signal_strength u16, reserved, checksum. Little-endian.
const FRAME_LEN: usize = 16;
const HEADER: u8 = 0x57;
const MARK_FRAME0: u8 = 0x00;
const MARK_READ: u8 = 0x10;
/// Bytes kept while waiting for the rest of a frame; older bytes are dropped.
const MAX_PENDING: usize = 256;
/// Readings emitted for one input message (the host allows 16 outputs per message, one is left
/// free); older complete frames in the same chunk are skipped.
const MAX_READINGS: usize = 15;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub id: u8,
    pub system_time_ms: u32,
    /// Metres; the sensor sends millimetres as a signed 24-bit value.
    pub distance_m: f64,
    pub status: u8,
    pub signal: u16,
}

/// The checksum is the low byte of the sum of all preceding bytes (manual Q15).
pub fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |sum, b| sum.wrapping_add(*b))
}

/// Decode one complete 16-byte frame; `None` if the header or checksum is wrong.
pub fn decode_frame(frame: &[u8]) -> Option<Reading> {
    if frame.len() != FRAME_LEN || frame[0] != HEADER || frame[1] != MARK_FRAME0 {
        return None;
    }
    if checksum(&frame[..FRAME_LEN - 1]) != frame[FRAME_LEN - 1] {
        return None;
    }
    // int24 → int32 keeping the sign: shift into the top three bytes, then arithmetic shift back.
    let raw = i32::from_le_bytes([0, frame[8], frame[9], frame[10]]) >> 8;
    Some(Reading {
        id: frame[3],
        system_time_ms: u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]),
        distance_m: f64::from(raw) / 1000.0,
        status: frame[11],
        signal: u16::from_le_bytes([frame[12], frame[13]]),
    })
}

/// `NLink_TOFSense_Read_Frame0` for module `id`: 57 10 FF FF id FF FF checksum.
pub fn read_frame(id: u8) -> [u8; 8] {
    let mut frame = [HEADER, MARK_READ, 0xFF, 0xFF, id, 0xFF, 0xFF, 0];
    frame[7] = checksum(&frame[..7]);
    frame
}

/// Stream reassembly: bytes may arrive split or several frames at once.
#[derive(Debug, Default)]
pub struct Assembler {
    pending: Vec<u8>,
    pub checksum_errors: u64,
}

impl Assembler {
    /// Append bytes and return every complete, checksum-valid frame found.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Reading> {
        self.pending.extend_from_slice(bytes);
        let mut readings = Vec::new();
        let mut start = 0;
        while self.pending.len() - start >= FRAME_LEN {
            let window = &self.pending[start..];
            if window[0] != HEADER || window[1] != MARK_FRAME0 {
                start += 1;
                continue;
            }
            match decode_frame(&window[..FRAME_LEN]) {
                Some(reading) => {
                    readings.push(reading);
                    start += FRAME_LEN;
                }
                None => {
                    // Bad checksum: resynchronise on the next header byte.
                    self.checksum_errors += 1;
                    start += 1;
                }
            }
        }
        self.pending.drain(..start);
        if self.pending.len() > MAX_PENDING {
            let excess = self.pending.len() - MAX_PENDING;
            self.pending.drain(..excess);
        }
        readings
    }
}

pub struct TofSense {
    millimetres: bool,
    min_signal: u16,
    valid_only: bool,
    sensor_id: Option<u8>,
    stream: Assembler,
    skipped: u64,
}

/// Bytes from a Buffer, or from an array of byte values (what `tcp in`/`tcp request` emit).
fn payload_bytes(value: Option<&EveValue>) -> Result<Vec<u8>, String> {
    match value {
        Some(EveValue::Bytes(bytes)) => Ok(bytes.clone()),
        Some(EveValue::Array(items)) => items
            .iter()
            .map(|item| match item {
                EveValue::I64(n) => u8::try_from(*n).map_err(|_| format!("byte value {n} out of range")),
                EveValue::U64(n) => u8::try_from(*n).map_err(|_| format!("byte value {n} out of range")),
                EveValue::F64(n) if n.fract() == 0.0 && (0.0..=255.0).contains(n) => Ok(*n as u8),
                _ => Err("payload array must contain byte values 0-255".to_string()),
            })
            .collect(),
        _ => Err("payload must be the sensor bytes (Buffer or array of byte values)".to_string()),
    }
}

impl TofSense {
    fn is_valid(&self, reading: &Reading) -> bool {
        reading.status == 0 && reading.signal >= self.min_signal && reading.distance_m >= 0.0
    }

    fn reading_msg(&self, input: &Msg, reading: &Reading) -> Msg {
        let distance = if self.millimetres { (reading.distance_m * 1000.0).round() } else { reading.distance_m };
        let mut msg = input.clone();
        msg.set_payload(EveValue::F64(distance));
        msg.set(
            "tof",
            EveValue::Object(vec![
                ("id".into(), EveValue::I64(i64::from(reading.id))),
                ("distance".into(), EveValue::F64(distance)),
                ("unit".into(), EveValue::String(if self.millimetres { "mm" } else { "m" }.into())),
                ("status".into(), EveValue::I64(i64::from(reading.status))),
                ("signal".into(), EveValue::I64(i64::from(reading.signal))),
                ("systemTimeMs".into(), EveValue::I64(i64::from(reading.system_time_ms))),
                ("valid".into(), EveValue::Bool(self.is_valid(reading))),
            ]),
        );
        msg
    }
}

impl Node for TofSense {
    fn init(config: &Msg) -> Result<Self, String> {
        let sensor_id = match config.get_i64("sensorId").unwrap_or(-1) {
            -1 => None,
            id => Some(u8::try_from(id).map_err(|_| "sensorId must be -1 or 0-255".to_string())?),
        };
        Ok(Self {
            millimetres: config.get_str("unit") == Some("mm"),
            min_signal: u16::try_from(config.get_i64("minSignal").unwrap_or(0))
                .map_err(|_| "minSignal out of range")?,
            valid_only: config.get_bool("validOnly").unwrap_or(false),
            sensor_id,
            stream: Assembler::default(),
            skipped: 0,
        })
    }

    fn on_input(&mut self, ctx: &mut Ctx, msg: Msg) -> Result<(), String> {
        // Query mode: build the read frame for one module.
        if let Some(id) = msg.get_i64("query") {
            let id = u8::try_from(id).map_err(|_| format!("query id {id} is not 0-255"))?;
            let mut out = msg.clone();
            out.remove("query");
            let frame = read_frame(id).iter().map(|b| EveValue::I64(i64::from(*b))).collect();
            out.set_payload(EveValue::Array(frame));
            return ctx.emit(1, &out);
        }

        let bytes = payload_bytes(msg.payload())?;
        let errors_before = self.stream.checksum_errors;
        let mut readings: Vec<Reading> = self
            .stream
            .push(&bytes)
            .into_iter()
            .filter(|r| self.sensor_id.is_none_or(|id| id == r.id))
            .filter(|r| !self.valid_only || self.is_valid(r))
            .collect();
        if readings.len() > MAX_READINGS {
            self.skipped += (readings.len() - MAX_READINGS) as u64;
            readings.drain(..readings.len() - MAX_READINGS);
        }
        for reading in &readings {
            ctx.emit(0, &self.reading_msg(&msg, reading))?;
        }
        if let Some(last) = readings.last() {
            let shown = if self.millimetres {
                format!("{:.0} mm", last.distance_m * 1000.0)
            } else {
                format!("{:.3} m", last.distance_m)
            };
            if self.is_valid(last) {
                ctx.status(Fill::Green, Shape::Dot, &format!("{shown} (id {})", last.id));
            } else {
                ctx.status(
                    Fill::Yellow,
                    Shape::Ring,
                    &format!("out of range (status {}, signal {})", last.status, last.signal),
                );
            }
        } else if self.stream.checksum_errors > errors_before {
            ctx.status(Fill::Red, Shape::Ring, &format!("{} checksum errors", self.stream.checksum_errors));
        }
        Ok(())
    }
}

export_node!(TofSense);

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual §8.2.1, table 1: 2.221 m, id 0, 36766 ms, status 0, signal 3.
    const MANUAL_FRAME: [u8; 16] =
        [0x57, 0x00, 0xff, 0x00, 0x9e, 0x8f, 0x00, 0x00, 0xad, 0x08, 0x00, 0x00, 0x03, 0x00, 0xff, 0x3a];

    fn config(pairs: &[(&str, EveValue)]) -> Msg {
        let mut msg = Msg::new();
        for (key, value) in pairs {
            msg.set(key, value.clone());
        }
        msg
    }

    fn bytes_msg(bytes: &[u8]) -> Msg {
        let mut msg = Msg::new();
        msg.set("_msgid", EveValue::String("m1".into()));
        msg.set_payload(EveValue::Array(bytes.iter().map(|b| EveValue::I64(i64::from(*b))).collect()));
        msg
    }

    #[test]
    fn decodes_the_manual_example() {
        let reading = decode_frame(&MANUAL_FRAME).unwrap();
        assert_eq!(reading, Reading { id: 0, system_time_ms: 36766, distance_m: 2.221, status: 0, signal: 3 });
        let mut corrupt = MANUAL_FRAME;
        corrupt[9] ^= 1;
        assert_eq!(decode_frame(&corrupt), None);
    }

    #[test]
    fn builds_the_manual_query_frame() {
        // Manual §8.2.2, table 2.
        assert_eq!(read_frame(0), [0x57, 0x10, 0xff, 0xff, 0x00, 0xff, 0xff, 0x63]);
        assert_eq!(checksum(&[0x55, 0x01, 0x00, 0xef, 0x03]), 0x48); // manual Q15
    }

    #[test]
    fn negative_int24_keeps_its_sign() {
        // Short range, out of range: -0.01 m = 0xFFFFF6 (manual Q10).
        let mut frame = MANUAL_FRAME;
        frame[8..11].copy_from_slice(&[0xf6, 0xff, 0xff]);
        frame[15] = checksum(&frame[..15]);
        assert_eq!(decode_frame(&frame).unwrap().distance_m, -0.01);
    }

    #[test]
    fn reassembles_split_frames_and_resyncs_after_garbage() {
        let mut stream = Assembler::default();
        assert!(stream.push(&MANUAL_FRAME[..5]).is_empty());
        assert_eq!(stream.push(&MANUAL_FRAME[5..]).len(), 1);
        let mut noisy = vec![0x01, 0x57, 0x02];
        let mut bad = MANUAL_FRAME;
        bad[15] ^= 0xff;
        noisy.extend_from_slice(&bad);
        noisy.extend_from_slice(&MANUAL_FRAME);
        noisy.extend_from_slice(&MANUAL_FRAME);
        assert_eq!(stream.push(&noisy).len(), 2);
        assert_eq!(stream.checksum_errors, 1);
    }

    #[test]
    fn node_emits_readings_queries_and_filters() {
        let mut node = TofSense::init(&config(&[("unit", EveValue::String("mm".into()))])).unwrap();
        let mut ctx = Ctx::new();
        node.on_input(&mut ctx, bytes_msg(&MANUAL_FRAME)).unwrap();
        let (port, out) = &ctx.record.outputs[0];
        assert_eq!(*port, 0);
        assert_eq!(out.payload(), Some(&EveValue::F64(2221.0)));
        assert_eq!(out.get_str("_msgid"), Some("m1"));
        assert_eq!(ctx.record.status.as_ref().unwrap().2, "2221 mm (id 0)");

        let mut ctx = Ctx::new();
        node.on_input(&mut ctx, config(&[("query", EveValue::I64(3))])).unwrap();
        let (port, out) = &ctx.record.outputs[0];
        assert_eq!(*port, 1);
        let EveValue::Array(frame) = out.payload().unwrap() else { panic!("array") };
        assert_eq!(frame[4], EveValue::I64(3));
        assert!(out.get("query").is_none());

        // Signal 3 is below 10: dropped when only valid readings are wanted.
        let mut strict =
            TofSense::init(&config(&[("minSignal", EveValue::I64(10)), ("validOnly", EveValue::Bool(true))])).unwrap();
        let mut ctx = Ctx::new();
        strict.on_input(&mut ctx, bytes_msg(&MANUAL_FRAME)).unwrap();
        assert!(ctx.record.outputs.is_empty());

        // Another module id is filtered out.
        let mut only_two = TofSense::init(&config(&[("sensorId", EveValue::I64(2))])).unwrap();
        let mut ctx = Ctx::new();
        only_two.on_input(&mut ctx, bytes_msg(&MANUAL_FRAME)).unwrap();
        assert!(ctx.record.outputs.is_empty());
        assert!(node.on_input(&mut Ctx::new(), config(&[("payload", EveValue::String("x".into()))])).is_err());
    }
}
