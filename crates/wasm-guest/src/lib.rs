//! Guest helpers for ABI `edgelink:node/v1`.
//!
//! The raw imports exist only when compiling for `wasm32`. On other targets this crate only
//! re-exports the EVE/1 codec, so host-side tests can encode and decode guest messages.
#![no_std]
extern crate alloc;

pub use edgelink_eve::{decode, encode, EveValue, Limits};

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

/// Emit an EVE/1-encoded message on `port`. A bound violation traps the call on the host.
#[cfg(target_arch = "wasm32")]
pub fn emit_bytes(port: u8, bytes: &[u8]) -> i32 {
    // SAFETY: the pointer and length describe a live slice in this module's linear memory; the
    // host only reads `len` bytes from it during the call.
    unsafe { imports::emit(i32::from(port), bytes.as_ptr() as i32, bytes.len() as i32) }
}

/// Log a line at `level` (0 debug, 1 info, 2 warn, 3 error); at most 512 bytes.
#[cfg(target_arch = "wasm32")]
pub fn log(level: u8, text: &str) -> i32 {
    // SAFETY: as for `emit_bytes`.
    unsafe { imports::log(i32::from(level), text.as_ptr() as i32, text.len() as i32) }
}

/// Set the node status: fill 0 red, 1 green, 2 yellow, 3 blue, 4 grey; shape 0 ring, 1 dot.
#[cfg(target_arch = "wasm32")]
pub fn status(fill: u8, shape: u8, text: &str) -> i32 {
    // SAFETY: as for `emit_bytes`.
    unsafe { imports::status(i32::from(fill), i32::from(shape), text.as_ptr() as i32, text.len() as i32) }
}

/// Fail the current message with `text` (at most 1 KiB); return non-zero from `el_on_input` too.
#[cfg(target_arch = "wasm32")]
pub fn fail(text: &str) -> i32 {
    // SAFETY: as for `emit_bytes`.
    unsafe { imports::fail(text.as_ptr() as i32, text.len() as i32) }
}
