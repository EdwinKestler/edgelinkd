//! Wasmi 2.0 backend: interpreter, fuel sliced through resumable calls so the host can
//! check a wall-clock deadline (or a cancellation flag) between slices.

use std::time::Instant;
use wasmi::{
    Caller, CompilationMode, Config, Extern, Linker, Module, Store, StoreLimits, StoreLimitsBuilder, TypedFunc,
    TypedResumableCall,
};

use crate::{fuel_per_call, DEADLINE, FUEL_SLICE, MAX_EMIT, MAX_EMITS, MEM_LIMIT};

pub const NAME: &str = "wasmi-2.0.0";

pub struct State {
    limits: StoreLimits,
    out: Vec<Vec<u8>>,
}

pub struct Engine {
    engine: wasmi::Engine,
    linker: Linker<State>,
}

pub struct Instance {
    store: Store<State>,
    alloc: TypedFunc<i32, i32>,
    on_input: TypedFunc<(i32, i32), i32>,
    memory: wasmi::Memory,
}

fn emit(mut caller: Caller<'_, State>, port: i32, ptr: i32, len: i32) -> Result<i32, wasmi::Error> {
    if port != 0 {
        return Err(wasmi::Error::new(format!("emit: no output port {port}")));
    }
    let len = usize::try_from(len).map_err(|_| wasmi::Error::new("emit: negative length"))?;
    if len > MAX_EMIT {
        return Err(wasmi::Error::new(format!("emit: {len} bytes exceeds {MAX_EMIT}")));
    }
    if caller.data().out.len() >= MAX_EMITS {
        return Err(wasmi::Error::new("emit: too many outputs"));
    }
    let Some(Extern::Memory(memory)) = caller.get_export("memory") else {
        return Err(wasmi::Error::new("emit: no exported memory"));
    };
    let mut buf = vec![0u8; len];
    memory
        .read(&caller, ptr as u32 as usize, &mut buf)
        .map_err(|_| wasmi::Error::new("emit: range outside linear memory"))?;
    caller.data_mut().out.push(buf);
    Ok(0)
}

impl Engine {
    pub fn new() -> Result<Self, String> {
        let mut config = Config::default();
        config
            .consume_fuel(true)
            .compilation_mode(CompilationMode::Eager)
            .allow_start_fn(false)
            // SIMD and memory64 are not compiled in (features off); multi-memory/tail calls off.
            .wasm_multi_memory(false)
            .wasm_tail_call(false)
            .set_max_recursion_depth(256);
        let engine = wasmi::Engine::new(&config);
        let mut linker = Linker::new(&engine);
        linker.func_wrap("edgelink:node/v1", "emit", emit).map_err(|e| e.to_string())?;
        Ok(Self { engine, linker })
    }

    pub fn compile(&self, bytes: &[u8]) -> Result<Module, String> {
        Module::new(&self.engine, bytes).map_err(|e| format!("compile: {e}"))
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
        store.set_fuel(FUEL_SLICE).map_err(|e| e.to_string())?;
        let instance =
            self.linker.instantiate_and_start(&mut store, module).map_err(|e| format!("instantiate: {e}"))?;
        let abi: TypedFunc<(), i32> =
            instance.get_typed_func(&store, "el_abi_version").map_err(|e| format!("abi: {e}"))?;
        if abi.call(&mut store, ()).map_err(|e| e.to_string())? != 1 {
            return Err("unsupported ABI".into());
        }
        Ok(Instance {
            alloc: instance.get_typed_func(&store, "el_alloc").map_err(|e| format!("el_alloc: {e}"))?,
            on_input: instance.get_typed_func(&store, "el_on_input").map_err(|e| format!("el_on_input: {e}"))?,
            memory: instance.get_memory(&store, "memory").ok_or("no exported memory")?,
            store,
        })
    }
}

impl Instance {
    pub fn on_input(&mut self, input: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        self.store.data_mut().out.clear();
        self.store.set_fuel(FUEL_SLICE).map_err(|e| e.to_string())?;
        let ptr = self.alloc.call(&mut self.store, input.len() as i32).map_err(|e| format!("el_alloc: {e}"))?;
        self.memory
            .write(&mut self.store, ptr as u32 as usize, input)
            .map_err(|_| "el_alloc returned a range outside linear memory".to_string())?;
        let deadline = Instant::now() + DEADLINE;
        let mut granted = FUEL_SLICE;
        self.store.set_fuel(FUEL_SLICE).map_err(|e| e.to_string())?;
        let mut call = self
            .on_input
            .call_resumable(&mut self.store, (ptr, input.len() as i32))
            .map_err(|e| format!("trap: {e}"))?;
        loop {
            match call {
                TypedResumableCall::Finished(0) => return Ok(std::mem::take(&mut self.store.data_mut().out)),
                TypedResumableCall::Finished(code) => return Err(format!("guest error code {code}")),
                TypedResumableCall::HostTrap(trap) => return Err(format!("host call failed: {}", trap.host_error())),
                TypedResumableCall::OutOfFuel(pending) => {
                    if granted >= fuel_per_call() {
                        return Err(format!("fuel budget {} exhausted", fuel_per_call()));
                    }
                    if Instant::now() >= deadline {
                        return Err(format!("deadline {} ms exceeded after {granted} fuel", DEADLINE.as_millis()));
                    }
                    let slice = FUEL_SLICE.max(pending.required_fuel());
                    granted += slice;
                    self.store.set_fuel(slice).map_err(|e| e.to_string())?;
                    call = pending.resume(&mut self.store).map_err(|e| format!("trap: {e}"))?;
                }
            }
        }
    }
}
