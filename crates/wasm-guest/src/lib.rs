//! Guest SDK for EdgeLinkd WASM plugins (ABI `edgelink:node/v1`).
//!
//! A plugin is a `cdylib` built for `wasm32-unknown-unknown` that implements [`Node`] and calls
//! [`export_node!`] and [`manifest!`]:
//!
//! ```ignore
//! use edgelink_wasm_guest::{export_node, manifest, Ctx, EveValue, Msg, Node};
//!
//! manifest!("../plugin.toml");
//!
//! struct Upper;
//! impl Node for Upper {
//!     fn init(_config: &Msg) -> Result<Self, String> { Ok(Upper) }
//!     fn on_input(&mut self, ctx: &mut Ctx, mut msg: Msg) -> Result<(), String> {
//!         if let Some(EveValue::String(text)) = msg.payload() {
//!             let upper = text.to_uppercase();
//!             msg.set_payload(EveValue::String(upper));
//!         }
//!         ctx.emit(0, &msg)
//!     }
//! }
//! export_node!(Upper);
//! ```
//!
//! `cargo build --release --target wasm32-unknown-unknown` then produces an installable package:
//! the manifest is embedded as the `edgelink.manifest` custom section. Link with
//! `-C link-arg=-zstack-size=65536` so the module starts within the default 512 KiB memory cap.
//!
//! On other targets the host imports do not exist: [`Ctx`] records what a plugin emits, logs and
//! reports, so plugin logic can be unit-tested with a plain `cargo test`.
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub use edgelink_eve::{decode, encode, EveValue, Limits};

/// Host bounds (`exec.rs`); the SDK checks them first so a plugin gets an `Err`, not a trap.
pub const MAX_EMIT_BYTES: usize = 64 * 1024;
pub const MAX_EMITS: usize = 16;
pub const MAX_EMIT_TOTAL: usize = 256 * 1024;
pub const MAX_LOG_BYTES: usize = 512;
pub const MAX_LOGS: usize = 16;
pub const MAX_STATUS_BYTES: usize = 128;
pub const MAX_FAIL_BYTES: usize = 1024;

#[cfg(target_arch = "wasm32")]
mod imports {
    #[link(wasm_import_module = "edgelink:node/v1")]
    unsafe extern "C" {
        pub fn emit(port: i32, ptr: i32, len: i32) -> i32;
        pub fn log(level: i32, ptr: i32, len: i32) -> i32;
        pub fn status(fill: i32, shape: i32, ptr: i32, len: i32) -> i32;
        pub fn fail(ptr: i32, len: i32) -> i32;
    }
}

/// The longest prefix of `text` that fits in `max` bytes without splitting a character.
fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Emit an EVE/1-encoded message on `port`. A bound violation faults the call on the host.
#[cfg(target_arch = "wasm32")]
pub fn emit_bytes(port: u8, bytes: &[u8]) -> i32 {
    // SAFETY: the pointer and length describe a live slice in this module's linear memory; the
    // host only reads `len` bytes from it during the call.
    unsafe { imports::emit(i32::from(port), bytes.as_ptr() as i32, bytes.len() as i32) }
}

/// Log a line at `level` (0 debug, 1 info, 2 warn, 3 error); at most 512 bytes.
#[cfg(target_arch = "wasm32")]
pub fn log(level: u8, text: &str) -> i32 {
    let text = clip(text, MAX_LOG_BYTES);
    // SAFETY: as for `emit_bytes`.
    unsafe { imports::log(i32::from(level), text.as_ptr() as i32, text.len() as i32) }
}

/// Set the node status: fill 0 red, 1 green, 2 yellow, 3 blue, 4 grey; shape 0 ring, 1 dot.
#[cfg(target_arch = "wasm32")]
pub fn status(fill: u8, shape: u8, text: &str) -> i32 {
    let text = clip(text, MAX_STATUS_BYTES);
    // SAFETY: as for `emit_bytes`.
    unsafe { imports::status(i32::from(fill), i32::from(shape), text.as_ptr() as i32, text.len() as i32) }
}

/// Fail the current message with `text` (at most 1 KiB); return non-zero from `el_on_input` too.
#[cfg(target_arch = "wasm32")]
pub fn fail(text: &str) -> i32 {
    let text = clip(text, MAX_FAIL_BYTES);
    // SAFETY: as for `emit_bytes`.
    unsafe { imports::fail(text.as_ptr() as i32, text.len() as i32) }
}

/// A message body (or the node configuration): an object with ordered, unique keys.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Msg {
    fields: Vec<(String, EveValue)>,
}

impl Msg {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode an EVE/1 object.
    pub fn from_eve(bytes: &[u8]) -> Result<Self, String> {
        match decode(bytes, Limits::default()) {
            Ok(EveValue::Object(fields)) => Ok(Self { fields }),
            Ok(_) => Err("message is not an object".to_string()),
            Err(err) => Err(err.to_string()),
        }
    }

    pub fn to_eve(&self) -> Result<Vec<u8>, String> {
        encode(&EveValue::Object(self.fields.clone())).map_err(|err| err.to_string())
    }

    pub fn fields(&self) -> &[(String, EveValue)] {
        &self.fields
    }

    pub fn get(&self, key: &str) -> Option<&EveValue> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Insert or replace `key`, keeping the position of an existing key.
    pub fn set(&mut self, key: &str, value: EveValue) {
        match self.fields.iter_mut().find(|(k, _)| k == key) {
            Some((_, slot)) => *slot = value,
            None => self.fields.push((key.to_string(), value)),
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<EveValue> {
        let index = self.fields.iter().position(|(k, _)| k == key)?;
        Some(self.fields.remove(index).1)
    }

    pub fn payload(&self) -> Option<&EveValue> {
        self.get("payload")
    }

    pub fn set_payload(&mut self, value: EveValue) {
        self.set("payload", value);
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        match self.get(key) {
            Some(EveValue::String(text)) => Some(text),
            _ => None,
        }
    }

    pub fn get_i64(&self, key: &str) -> Option<i64> {
        match self.get(key) {
            Some(EveValue::I64(n)) => Some(*n),
            Some(EveValue::U64(n)) => i64::try_from(*n).ok(),
            _ => None,
        }
    }

    pub fn get_f64(&self, key: &str) -> Option<f64> {
        match self.get(key) {
            Some(EveValue::F64(n)) => Some(*n),
            Some(EveValue::I64(n)) => Some(*n as f64),
            Some(EveValue::U64(n)) => Some(*n as f64),
            _ => None,
        }
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match self.get(key) {
            Some(EveValue::Bool(flag)) => Some(*flag),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    Red = 0,
    Green = 1,
    Yellow = 2,
    Blue = 3,
    Grey = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Ring = 0,
    Dot = 1,
}

/// What a plugin can do while handling one message. Outputs reach the flow only if
/// [`Node::on_input`] returns `Ok`.
#[derive(Debug, Default)]
pub struct Ctx {
    emits: usize,
    emitted_bytes: usize,
    logs: usize,
    /// Off `wasm32`: everything the plugin emitted, logged and reported, for tests.
    #[cfg(not(target_arch = "wasm32"))]
    pub record: Record,
}

/// Recorded calls (only off `wasm32`).
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Record {
    pub outputs: Vec<(u8, Msg)>,
    pub logs: Vec<(Level, String)>,
    pub status: Option<(Fill, Shape, String)>,
}

impl Ctx {
    pub fn new() -> Self {
        Self::default()
    }

    /// Send `msg` on output `port` (0-based, below the manifest's `outputs`).
    pub fn emit(&mut self, port: u8, msg: &Msg) -> Result<(), String> {
        let bytes = msg.to_eve()?;
        if bytes.len() > MAX_EMIT_BYTES {
            return Err(format!("output of {} bytes exceeds {MAX_EMIT_BYTES}", bytes.len()));
        }
        if self.emits >= MAX_EMITS {
            return Err(format!("more than {MAX_EMITS} outputs for one message"));
        }
        if self.emitted_bytes + bytes.len() > MAX_EMIT_TOTAL {
            return Err(format!("outputs for one message exceed {MAX_EMIT_TOTAL} bytes"));
        }
        self.emits += 1;
        self.emitted_bytes += bytes.len();
        #[cfg(target_arch = "wasm32")]
        emit_bytes(port, &bytes);
        #[cfg(not(target_arch = "wasm32"))]
        self.record.outputs.push((port, msg.clone()));
        Ok(())
    }

    /// Log a line (at most 512 bytes; lines beyond 16 per message are dropped).
    pub fn log(&mut self, level: Level, text: &str) {
        if self.logs >= MAX_LOGS {
            return;
        }
        self.logs += 1;
        #[cfg(target_arch = "wasm32")]
        log(level as u8, text);
        #[cfg(not(target_arch = "wasm32"))]
        self.record.logs.push((level, clip(text, MAX_LOG_BYTES).to_string()));
    }

    /// Set the node status shown under it in the editor (text at most 128 bytes).
    pub fn status(&mut self, fill: Fill, shape: Shape, text: &str) {
        #[cfg(target_arch = "wasm32")]
        status(fill as u8, shape as u8, text);
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.record.status = Some((fill, shape, clip(text, MAX_STATUS_BYTES).to_string()));
        }
    }
}

/// A plugin node. One value lives per flow node instance; it is created by [`Node::init`] with
/// the node's configuration and keeps its state between messages.
pub trait Node: Sized {
    /// The resolved `[[node.config]]` object. Return `Err` to reject the configuration.
    fn init(config: &Msg) -> Result<Self, String>;
    /// Handle one message. `Err` fails the message (catchable in the flow); nothing emitted for
    /// it reaches the flow.
    fn on_input(&mut self, ctx: &mut Ctx, msg: Msg) -> Result<(), String>;
    /// Called when the node stops (not after a fault). No outputs are possible here.
    fn close(&mut self) {}
}

/// Runtime behind [`export_node!`]. Not part of the public API.
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub mod __rt {
    use super::*;
    use alloc::vec;
    use core::cell::UnsafeCell;

    pub struct Slot<T>(UnsafeCell<Option<T>>);

    // SAFETY: `wasm32-unknown-unknown` guests are single-threaded, and the host never calls into
    // a guest while another call into it is running.
    unsafe impl<T> Sync for Slot<T> {}

    impl<T> Slot<T> {
        pub const fn new() -> Self {
            Self(UnsafeCell::new(None))
        }

        #[allow(clippy::mut_from_ref)]
        fn get(&self) -> &mut Option<T> {
            // SAFETY: see the `Sync` impl; no two references are live at the same time.
            unsafe { &mut *self.0.get() }
        }
    }

    impl<T> Default for Slot<T> {
        fn default() -> Self {
            Self::new()
        }
    }

    static INPUT: Slot<Vec<u8>> = Slot::new();

    pub fn alloc(len: i32) -> i32 {
        let buffer = INPUT.get().insert(vec![0u8; len.max(0) as usize]);
        buffer.as_mut_ptr() as i32
    }

    fn take_input(ptr: i32, len: i32) -> Result<Vec<u8>, String> {
        let buffer = INPUT.get().take().ok_or_else(|| "el_alloc was not called".to_string())?;
        if buffer.as_ptr() as i32 != ptr || buffer.len() != len.max(0) as usize {
            return Err("input does not match the el_alloc buffer".to_string());
        }
        Ok(buffer)
    }

    fn failed(text: &str) -> i32 {
        fail(text);
        1
    }

    pub fn init<N: Node>(slot: &Slot<N>, ptr: i32, len: i32) -> i32 {
        match take_input(ptr, len).and_then(|bytes| Msg::from_eve(&bytes)).and_then(|config| N::init(&config)) {
            Ok(node) => {
                *slot.get() = Some(node);
                0
            }
            Err(text) => failed(&text),
        }
    }

    pub fn on_input<N: Node>(slot: &Slot<N>, ptr: i32, len: i32) -> i32 {
        let msg = match take_input(ptr, len).and_then(|bytes| Msg::from_eve(&bytes)) {
            Ok(msg) => msg,
            Err(text) => return failed(&text),
        };
        let Some(node) = slot.get().as_mut() else {
            return failed("el_on_input before el_init");
        };
        match node.on_input(&mut Ctx::new(), msg) {
            Ok(()) => 0,
            Err(text) => failed(&text),
        }
    }

    pub fn close<N: Node>(slot: &Slot<N>) {
        if let Some(mut node) = slot.get().take() {
            node.close();
        }
    }
}

/// Export ABI v1 (`el_abi_version`, `el_alloc`, `el_init`, `el_on_input`, `el_close`) for a
/// [`Node`] type. Expands to nothing off `wasm32`.
#[macro_export]
macro_rules! export_node {
    ($node:ty) => {
        #[cfg(target_arch = "wasm32")]
        const _: () = {
            static NODE: $crate::__rt::Slot<$node> = $crate::__rt::Slot::new();

            #[no_mangle]
            pub extern "C" fn el_abi_version() -> i32 {
                1
            }

            #[no_mangle]
            pub extern "C" fn el_alloc(len: i32) -> i32 {
                $crate::__rt::alloc(len)
            }

            #[no_mangle]
            pub extern "C" fn el_init(ptr: i32, len: i32) -> i32 {
                $crate::__rt::init::<$node>(&NODE, ptr, len)
            }

            #[no_mangle]
            pub extern "C" fn el_on_input(ptr: i32, len: i32) -> i32 {
                $crate::__rt::on_input::<$node>(&NODE, ptr, len)
            }

            #[no_mangle]
            pub extern "C" fn el_close() {
                $crate::__rt::close::<$node>(&NODE)
            }
        };
    };
}

/// Embed a manifest file (path relative to the calling source file) as the
/// `edgelink.manifest` custom section. Expands to nothing off `wasm32`.
#[macro_export]
macro_rules! manifest {
    ($path:literal) => {
        #[cfg(target_arch = "wasm32")]
        #[link_section = "edgelink.manifest"]
        #[used]
        static EDGELINK_MANIFEST: [u8; include_bytes!($path).len()] = *include_bytes!($path);
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_through_eve() {
        let mut msg = Msg::new();
        msg.set("_msgid", EveValue::String("abc".into()));
        msg.set_payload(EveValue::I64(7));
        msg.set_payload(EveValue::String("x".into()));
        assert_eq!(msg.fields().len(), 2, "set replaces in place");
        let back = Msg::from_eve(&msg.to_eve().unwrap()).unwrap();
        assert_eq!(back, msg);
        assert_eq!(back.get_str("payload"), Some("x"));
        assert!(Msg::from_eve(&encode(&EveValue::I64(1)).unwrap()).is_err());
    }

    #[test]
    fn ctx_enforces_host_bounds_before_the_host_does() {
        let mut ctx = Ctx::new();
        let mut big = Msg::new();
        big.set_payload(EveValue::Bytes(alloc::vec![0; MAX_EMIT_BYTES]));
        assert!(ctx.emit(0, &big).unwrap_err().contains("exceeds"));
        let small = Msg::new();
        for _ in 0..MAX_EMITS {
            ctx.emit(0, &small).unwrap();
        }
        assert!(ctx.emit(0, &small).is_err());
        for i in 0..20 {
            ctx.log(Level::Info, &format!("line {i}"));
        }
        assert_eq!(ctx.record.logs.len(), MAX_LOGS);
        ctx.status(Fill::Green, Shape::Dot, &"é".repeat(100));
        assert!(ctx.record.status.as_ref().unwrap().2.len() <= MAX_STATUS_BYTES);
    }
}
