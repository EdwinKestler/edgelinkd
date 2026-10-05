//! Wasmtime backend (36 LTS or 48 LTS): fuel per call plus an epoch deadline advanced by a
//! 10 ms ticker thread, linear memory capped through `StoreLimits`, explicit bounds checks
//! (small reservation, no guard region) as an embedded configuration would use.

#[cfg(feature = "wt36")]
use wasmtime36 as wasmtime;
#[cfg(feature = "wt36-runtime-only")]
use wasmtime36_rt as wasmtime;
#[cfg(feature = "wt48")]
use wasmtime48 as wasmtime;

use std::time::Duration;
use wasmtime::{Caller, Config, Extern, Linker, Module, Store, StoreLimits, StoreLimitsBuilder, TypedFunc};

use crate::{fuel_per_call, DEADLINE, MAX_EMIT, MAX_EMITS, MEM_LIMIT};

#[cfg(all(feature = "wt36", not(feature = "wt36-pulley")))]
pub const NAME: &str = "wasmtime-36.0.17";
#[cfg(feature = "wt36-pulley")]
pub const NAME: &str = "wasmtime-36.0.17-pulley";
#[cfg(feature = "wt36-runtime-only")]
pub const NAME: &str = "wasmtime-36.0.17-runtime-only";
#[cfg(feature = "wt48")]
pub const NAME: &str = "wasmtime-48.0.5";

const EPOCH_TICK: Duration = Duration::from_millis(10);

pub struct State {
    limits: StoreLimits,
    out: Vec<Vec<u8>>,
}

pub struct Engine {
    engine: wasmtime::Engine,
    linker: Linker<State>,
}

pub struct Instance {
    store: Store<State>,
    alloc: TypedFunc<i32, i32>,
    on_input: TypedFunc<(i32, i32), i32>,
    memory: wasmtime::Memory,
}

fn err(msg: impl Into<String>) -> wasmtime::Error {
    wasmtime::Error::msg(msg.into())
}

fn emit(mut caller: Caller<'_, State>, port: i32, ptr: i32, len: i32) -> wasmtime::Result<i32> {
    if port != 0 {
        return Err(err(format!("emit: no output port {port}")));
    }
    let len = usize::try_from(len).map_err(|_| err("emit: negative length"))?;
    if len > MAX_EMIT {
        return Err(err(format!("emit: {len} bytes exceeds {MAX_EMIT}")));
    }
    if caller.data().out.len() >= MAX_EMITS {
        return Err(err("emit: too many outputs"));
    }
    let Some(Extern::Memory(memory)) = caller.get_export("memory") else {
        return Err(err("emit: no exported memory"));
    };
    let mut buf = vec![0u8; len];
    memory.read(&caller, ptr as u32 as usize, &mut buf).map_err(|_| err("emit: range outside linear memory"))?;
    caller.data_mut().out.push(buf);
    Ok(0)
}

fn config() -> Config {
    let mut c = Config::new();
    c.consume_fuel(true);
    c.epoch_interruption(true);
    c.max_wasm_stack(256 * 1024);
    // Embedded-style memory: no 4 GiB virtual reservation, explicit bounds checks.
    c.memory_reservation(MEM_LIMIT as u64);
    c.memory_guard_size(0);
    c.memory_reservation_for_growth(0);
    c.wasm_simd(false);
    c.wasm_relaxed_simd(false);
    #[cfg(all(feature = "wt36-pulley", target_pointer_width = "64"))]
    c.target("pulley64").expect("pulley64 target");
    #[cfg(all(feature = "wt36-pulley", target_pointer_width = "32"))]
    c.target("pulley32").expect("pulley32 target");
    c
}

impl Engine {
    pub fn new() -> Result<Self, String> {
        let engine = wasmtime::Engine::new(&config()).map_err(|e| e.to_string())?;
        let ticker = engine.clone();
        std::thread::Builder::new()
            .name("wasm-epoch".into())
            .spawn(move || loop {
                std::thread::sleep(EPOCH_TICK);
                ticker.increment_epoch();
            })
            .map_err(|e| e.to_string())?;
        let mut linker = Linker::new(&engine);
        linker.func_wrap("edgelink:node/v1", "emit", emit).map_err(|e| e.to_string())?;
        Ok(Self { engine, linker })
    }

    #[cfg(not(feature = "wt36-runtime-only"))]
    pub fn compile(&self, bytes: &[u8]) -> Result<Module, String> {
        Module::new(&self.engine, bytes).map_err(|e| format!("compile: {e:#}"))
    }

    /// Runtime-only build: can only load artifacts produced by the same Wasmtime version and
    /// configuration. `deserialize` is `unsafe` because the bytes are trusted native code.
    #[cfg(feature = "wt36-runtime-only")]
    pub fn compile(&self, bytes: &[u8]) -> Result<Module, String> {
        // SAFETY: spike only; the `.cwasm` files are produced locally by `precompile`.
        unsafe { Module::deserialize(&self.engine, bytes) }.map_err(|e| format!("deserialize: {e:#}"))
    }

    #[cfg(feature = "wt36")]
    pub fn precompile(&self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        self.engine.precompile_module(bytes).map_err(|e| format!("precompile: {e:#}"))
    }

    pub fn instantiate(&self, module: &Module) -> Result<Instance, String> {
        for import in module.imports() {
            if import.module() != "edgelink:node/v1" {
                return Err(format!("import {}::{} is not granted", import.module(), import.name()));
            }
        }
        let limits = StoreLimitsBuilder::new()
            .memory_size(MEM_LIMIT)
            .memories(1)
            .tables(1)
            .table_elements(1024)
            .instances(1)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(&self.engine, State { limits, out: Vec::new() });
        store.limiter(|s| &mut s.limits);
        store.set_fuel(fuel_per_call()).map_err(|e| e.to_string())?;
        store.set_epoch_deadline(ticks());
        let instance = self.linker.instantiate(&mut store, module).map_err(|e| format!("instantiate: {e:#}"))?;
        let abi: TypedFunc<(), i32> =
            instance.get_typed_func(&mut store, "el_abi_version").map_err(|e| format!("abi: {e:#}"))?;
        if abi.call(&mut store, ()).map_err(|e| format!("{e:#}"))? != 1 {
            return Err("unsupported ABI".into());
        }
        Ok(Instance {
            alloc: instance.get_typed_func(&mut store, "el_alloc").map_err(|e| format!("el_alloc: {e:#}"))?,
            on_input: instance.get_typed_func(&mut store, "el_on_input").map_err(|e| format!("el_on_input: {e:#}"))?,
            memory: instance.get_memory(&mut store, "memory").ok_or("no exported memory")?,
            store,
        })
    }
}

fn ticks() -> u64 {
    (DEADLINE.as_millis() / EPOCH_TICK.as_millis()).max(1) as u64
}

impl Instance {
    pub fn on_input(&mut self, input: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        self.store.data_mut().out.clear();
        self.store.set_fuel(fuel_per_call()).map_err(|e| e.to_string())?;
        self.store.set_epoch_deadline(ticks());
        let ptr = self.alloc.call(&mut self.store, input.len() as i32).map_err(|e| format!("el_alloc: {e:#}"))?;
        self.memory
            .write(&mut self.store, ptr as u32 as usize, input)
            .map_err(|_| "el_alloc returned a range outside linear memory".to_string())?;
        match self.on_input.call(&mut self.store, (ptr, input.len() as i32)) {
            Ok(0) => Ok(std::mem::take(&mut self.store.data_mut().out)),
            Ok(code) => Err(format!("guest error code {code}")),
            Err(e) => Err(format!("trap: {e:#}")),
        }
    }
}
