//! Fuel-sliced Wasmi execution for ABI `n2link:node/v1`.
//!
//! Only four host functions are linkable (`emit`, `log`, `status`, `fail`). Every call runs in
//! fuel slices so the host can check the fuel budget, the wall-clock deadline and cancellation
//! between slices (Wasmi has no epoch interruption).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use wasmi::{
    Caller, CompilationMode, Config, Extern, ExternType, Linker, Module, Store, StoreLimits, StoreLimitsBuilder,
    TypedFunc, TypedResumableCall, ValType, WasmResults,
};

use crate::N2linkError;

pub(crate) const ABI_MODULE: &str = "n2link:node/v1";
/// The import module n2link plugins used before the rename; such modules get a rebuild hint.
const LEGACY_ABI_MODULE: &str = "edgelink:node/v1";
use ValType::I32;

/// Host functions a guest may import, with their exact signatures.
const ABI_IMPORTS: [(&str, &[ValType], &[ValType]); 4] = [
    ("emit", &[I32, I32, I32], &[I32]),
    ("log", &[I32, I32, I32], &[I32]),
    ("status", &[I32, I32, I32, I32], &[I32]),
    ("fail", &[I32, I32], &[I32]),
];

/// Guest exports: (name, params, results, required). `el_init` receives the node configuration
/// (one EVE object); `el_close` runs when the node stops.
const ABI_EXPORTS: [(&str, &[ValType], &[ValType], bool); 5] = [
    ("el_abi_version", &[], &[I32], true),
    ("el_alloc", &[I32], &[I32], true),
    ("el_on_input", &[I32, I32], &[I32], true),
    ("el_init", &[I32, I32], &[I32], false),
    ("el_close", &[], &[], false),
];

fn signature(params: &[ValType], results: &[ValType]) -> String {
    format!("{params:?} -> {results:?}")
}

pub(crate) const MAX_EMIT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_EMITS: usize = 16;
pub(crate) const MAX_EMIT_TOTAL: usize = 256 * 1024;
pub(crate) const MAX_LOG_BYTES: usize = 512;
pub(crate) const MAX_LOGS: usize = 16;
pub(crate) const MAX_STATUS_BYTES: usize = 128;
pub(crate) const MAX_FAIL_BYTES: usize = 1024;

/// One Wasmi engine, its linker and the modules compiled for it.
pub(crate) struct EngineCell {
    engine: wasmi::Engine,
    linker: Linker<HostState>,
    modules: Mutex<HashMap<[u8; 32], Module>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuestLogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuestStatus {
    pub fill: u8,
    pub shape: u8,
    pub text: String,
}

pub(crate) struct HostState {
    limits: StoreLimits,
    outputs: Vec<(u8, Vec<u8>)>,
    logs: Vec<(GuestLogLevel, String)>,
    status: Option<GuestStatus>,
    fail: Option<String>,
    outputs_allowed: u8,
}

pub(crate) struct Budget {
    pub fuel: u64,
    pub slice: u64,
    pub deadline: Instant,
}

/// Everything a successful call produced, in call order.
#[derive(Debug, Default)]
pub(crate) struct CallOutput {
    pub outputs: Vec<(u8, Vec<u8>)>,
    pub logs: Vec<(GuestLogLevel, String)>,
    pub status: Option<GuestStatus>,
}

/// Why a call did not produce output.
#[derive(Debug)]
pub(crate) enum CallError {
    /// Stop/redeploy cancelled the call. Not a node error.
    Cancelled,
    /// The guest returned non-zero or called `fail`. The instance stays usable.
    Guest { text: String, logs: Vec<(GuestLogLevel, String)> },
    /// Fuel, deadline, trap, host bound or ABI misuse. The instance must be discarded.
    Fault(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Cancelled => write!(f, "cancelled"),
            CallError::Guest { text, .. } => write!(f, "{text}"),
            CallError::Fault(text) => write!(f, "{text}"),
        }
    }
}

pub(crate) struct Instance {
    store: Store<HostState>,
    alloc: TypedFunc<i32, i32>,
    on_input: TypedFunc<(i32, i32), i32>,
    init: Option<TypedFunc<(i32, i32), i32>>,
    close: Option<TypedFunc<(), ()>>,
    memory: wasmi::Memory,
}

fn host_error(msg: impl Into<String>) -> wasmi::Error {
    wasmi::Error::new(msg.into())
}

fn read_guest(
    caller: &Caller<'_, HostState>,
    what: &str,
    ptr: i32,
    len: i32,
    cap: usize,
) -> Result<Vec<u8>, wasmi::Error> {
    let len = usize::try_from(len).map_err(|_| host_error(format!("{what}: negative length")))?;
    if len > cap {
        return Err(host_error(format!("{what}: {len} bytes exceeds {cap}")));
    }
    let Some(Extern::Memory(memory)) = caller.get_export("memory") else {
        return Err(host_error(format!("{what}: no exported memory")));
    };
    let mut buf = vec![0u8; len];
    memory
        .read(caller, ptr as u32 as usize, &mut buf)
        .map_err(|_| host_error(format!("{what}: range outside linear memory")))?;
    Ok(buf)
}

fn guest_text(bytes: &[u8], cap: usize) -> String {
    String::from_utf8_lossy(bytes).chars().filter(|c| !c.is_control()).take(cap).collect()
}

fn emit(mut caller: Caller<'_, HostState>, port: i32, ptr: i32, len: i32) -> Result<i32, wasmi::Error> {
    let port = u8::try_from(port).map_err(|_| host_error(format!("emit: bad output port {port}")))?;
    if port >= caller.data().outputs_allowed {
        return Err(host_error(format!("emit: bad output port {port}")));
    }
    if caller.data().outputs.len() >= MAX_EMITS {
        return Err(host_error(format!("emit: more than {MAX_EMITS} outputs")));
    }
    let buf = read_guest(&caller, "emit", ptr, len, MAX_EMIT_BYTES)?;
    let sum: usize = caller.data().outputs.iter().map(|(_, b)| b.len()).sum();
    if sum.saturating_add(buf.len()) > MAX_EMIT_TOTAL {
        return Err(host_error(format!("emit: total output exceeds {MAX_EMIT_TOTAL} bytes")));
    }
    caller.data_mut().outputs.push((port, buf));
    Ok(0)
}

fn log_host(mut caller: Caller<'_, HostState>, level: i32, ptr: i32, len: i32) -> Result<i32, wasmi::Error> {
    let level = match level {
        0 => GuestLogLevel::Debug,
        1 => GuestLogLevel::Info,
        2 => GuestLogLevel::Warn,
        3 => GuestLogLevel::Error,
        other => return Err(host_error(format!("log: bad level {other}"))),
    };
    if caller.data().logs.len() >= MAX_LOGS {
        return Err(host_error(format!("log: more than {MAX_LOGS} lines")));
    }
    let buf = read_guest(&caller, "log", ptr, len, MAX_LOG_BYTES)?;
    caller.data_mut().logs.push((level, guest_text(&buf, MAX_LOG_BYTES)));
    Ok(0)
}

fn status_host(
    mut caller: Caller<'_, HostState>,
    fill: i32,
    shape: i32,
    ptr: i32,
    len: i32,
) -> Result<i32, wasmi::Error> {
    let fill =
        u8::try_from(fill).ok().filter(|f| *f <= 4).ok_or_else(|| host_error(format!("status: bad fill {fill}")))?;
    let shape =
        u8::try_from(shape).ok().filter(|s| *s <= 1).ok_or_else(|| host_error(format!("status: bad shape {shape}")))?;
    let buf = read_guest(&caller, "status", ptr, len, MAX_STATUS_BYTES)?;
    caller.data_mut().status = Some(GuestStatus { fill, shape, text: guest_text(&buf, MAX_STATUS_BYTES) });
    Ok(0)
}

fn fail_host(mut caller: Caller<'_, HostState>, ptr: i32, len: i32) -> Result<i32, wasmi::Error> {
    let buf = read_guest(&caller, "fail", ptr, len, MAX_FAIL_BYTES)?;
    caller.data_mut().fail = Some(guest_text(&buf, MAX_FAIL_BYTES));
    Ok(0)
}

fn config_error(err: impl std::fmt::Display) -> N2linkError {
    N2linkError::invalid_operation(&err.to_string())
}

impl EngineCell {
    pub(crate) fn new() -> crate::Result<Self> {
        let mut config = Config::default();
        config
            .consume_fuel(true)
            .compilation_mode(CompilationMode::Eager)
            .allow_start_fn(false)
            // ABI v1 allows f32/f64 (ordinary Rust guests emit them). SIMD and memory64 are not
            // compiled into this build (wasmi features), so such modules fail validation.
            .floats(true)
            .wasm_multi_memory(false)
            .wasm_tail_call(false)
            .set_max_recursion_depth(256);
        let engine = wasmi::Engine::new(&config);
        let mut linker = Linker::new(&engine);
        linker.func_wrap(ABI_MODULE, "emit", emit).map_err(config_error)?;
        linker.func_wrap(ABI_MODULE, "log", log_host).map_err(config_error)?;
        linker.func_wrap(ABI_MODULE, "status", status_host).map_err(config_error)?;
        linker.func_wrap(ABI_MODULE, "fail", fail_host).map_err(config_error)?;
        Ok(Self { engine, linker, modules: Mutex::new(HashMap::new()) })
    }

    /// Validate and compile once; reject any import outside ABI v1, and any missing or
    /// mistyped ABI export, before linking.
    pub(crate) fn compile(&self, bytes: &[u8]) -> crate::Result<Module> {
        let module = Module::new(&self.engine, bytes).map_err(|err| config_error(format!("compile: {err}")))?;
        for import in module.imports() {
            let granted = ABI_IMPORTS.iter().find(|(name, ..)| import.module() == ABI_MODULE && import.name() == *name);
            let Some((name, params, results)) = granted else {
                if import.module() == LEGACY_ABI_MODULE {
                    return Err(N2linkError::NotSupported(format!(
                        "import {}::{} is from EdgeLinkd; rebuild the plugin for ABI {ABI_MODULE}",
                        import.module(),
                        import.name()
                    )));
                }
                return Err(N2linkError::NotSupported(format!(
                    "import {}::{} is not granted by ABI {ABI_MODULE}",
                    import.module(),
                    import.name()
                )));
            };
            match import.ty() {
                ExternType::Func(ty) if ty.params() == *params && ty.results() == *results => {}
                ExternType::Func(ty) => {
                    return Err(N2linkError::NotSupported(format!(
                        "import {ABI_MODULE}::{name} has type {}, expected {}",
                        signature(ty.params(), ty.results()),
                        signature(params, results)
                    )));
                }
                _ => {
                    return Err(N2linkError::NotSupported(format!(
                        "import {ABI_MODULE}::{name} must be a function (imported memories, tables and globals are not granted)"
                    )));
                }
            }
        }
        let mut memory = false;
        let mut found = [false; ABI_EXPORTS.len()];
        for export in module.exports() {
            if export.name() == "memory" {
                memory = matches!(export.ty(), ExternType::Memory(_));
                continue;
            }
            let Some(index) = ABI_EXPORTS.iter().position(|(name, ..)| *name == export.name()) else {
                continue;
            };
            let (name, params, results, _) = ABI_EXPORTS[index];
            match export.ty() {
                ExternType::Func(ty) if ty.params() == params && ty.results() == results => found[index] = true,
                ExternType::Func(ty) => {
                    return Err(N2linkError::NotSupported(format!(
                        "export {name} has type {}, expected {}",
                        signature(ty.params(), ty.results()),
                        signature(params, results)
                    )));
                }
                _ => return Err(N2linkError::NotSupported(format!("export {name} must be a function"))),
            }
        }
        if !memory {
            return Err(N2linkError::NotSupported("plugin must export its linear memory as \"memory\"".to_owned()));
        }
        for (index, (name, _, _, required)) in ABI_EXPORTS.iter().enumerate() {
            if *required && !found[index] {
                return Err(N2linkError::NotSupported(format!("plugin does not export {name}")));
            }
        }
        Ok(module)
    }

    /// Initial size of the exported linear memory, in 64 KiB pages.
    pub(crate) fn min_memory_pages(module: &Module) -> u64 {
        module
            .exports()
            .find_map(|export| match (export.name(), export.ty()) {
                ("memory", ExternType::Memory(ty)) => Some(ty.minimum()),
                _ => None,
            })
            .unwrap_or(0)
    }

    /// Whether a compiled module exports `name` (for example `el_init`).
    pub(crate) fn exports(module: &Module, name: &str) -> bool {
        module.exports().any(|export| export.name() == name)
    }

    /// Compiled module for `sha256`, compiling on first use.
    pub(crate) fn module(&self, sha256: [u8; 32], bytes: &[u8]) -> crate::Result<Module> {
        if let Some(module) = self.modules.lock().unwrap_or_else(|e| e.into_inner()).get(&sha256) {
            return Ok(module.clone());
        }
        let module = self.compile(bytes)?;
        self.modules.lock().unwrap_or_else(|e| e.into_inner()).insert(sha256, module.clone());
        Ok(module)
    }

    pub(crate) fn instantiate(
        &self,
        module: &Module,
        memory_pages: u32,
        outputs_allowed: u8,
        budget: &Budget,
    ) -> crate::Result<Instance> {
        let limits = StoreLimitsBuilder::new()
            .memory_size((memory_pages as usize).saturating_mul(65536))
            .memories(1)
            .tables(1)
            .table_elements(10_000)
            .instances(1)
            .trap_on_grow_failure(true)
            .build();
        let state =
            HostState { limits, outputs: Vec::new(), logs: Vec::new(), status: None, fail: None, outputs_allowed };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|s| &mut s.limits);
        store.set_fuel(budget.slice).map_err(config_error)?;
        let instance = self
            .linker
            .instantiate_and_start(&mut store, module)
            .map_err(|err| config_error(format!("instantiate: {err}")))?;
        let abi: TypedFunc<(), i32> = instance
            .get_typed_func(&store, "el_abi_version")
            .map_err(|err| config_error(format!("el_abi_version: {err}")))?;
        let version = abi.call(&mut store, ()).map_err(|err| config_error(format!("el_abi_version: {err}")))?;
        if version != 1 {
            return Err(N2linkError::NotSupported(format!("unsupported WASM ABI {version}")));
        }
        Ok(Instance {
            alloc: instance
                .get_typed_func(&store, "el_alloc")
                .map_err(|err| config_error(format!("el_alloc: {err}")))?,
            on_input: instance
                .get_typed_func(&store, "el_on_input")
                .map_err(|err| config_error(format!("el_on_input: {err}")))?,
            init: instance.get_typed_func(&store, "el_init").ok(),
            close: instance.get_typed_func(&store, "el_close").ok(),
            memory: instance
                .get_memory(&store, "memory")
                .ok_or_else(|| N2linkError::invalid_operation("plugin does not export memory"))?,
            store,
        })
    }
}

impl Instance {
    fn reset(&mut self) {
        let state = self.store.data_mut();
        state.outputs.clear();
        state.logs.clear();
        state.status = None;
        state.fail = None;
    }

    /// Copy `input` into guest memory through `el_alloc`.
    fn write_input(&mut self, input: &[u8], budget: &Budget) -> Result<(i32, i32), CallError> {
        let len = i32::try_from(input.len()).map_err(|_| CallError::Fault("input too large".to_owned()))?;
        self.store.set_fuel(budget.slice).map_err(|err| CallError::Fault(err.to_string()))?;
        let ptr = self.alloc.call(&mut self.store, len).map_err(|err| CallError::Fault(format!("el_alloc: {err}")))?;
        self.memory
            .write(&mut self.store, ptr as u32 as usize, input)
            .map_err(|_| CallError::Fault("el_alloc returned a range outside linear memory".to_owned()))?;
        Ok((ptr, len))
    }

    /// Run a resumable call to completion in fuel slices, checking the fuel budget, the
    /// deadline and cancellation between slices.
    fn drive<R: WasmResults>(
        &mut self,
        mut call: TypedResumableCall<R>,
        budget: &Budget,
        cancel: &AtomicBool,
    ) -> Result<R, CallError> {
        let mut granted = budget.slice;
        loop {
            match call {
                TypedResumableCall::Finished(value) => return Ok(value),
                TypedResumableCall::HostTrap(trap) => return Err(CallError::Fault(trap.host_error().to_string())),
                TypedResumableCall::OutOfFuel(pending) => {
                    if cancel.load(Ordering::Acquire) {
                        return Err(CallError::Cancelled);
                    }
                    if granted >= budget.fuel {
                        return Err(CallError::Fault(format!("fuel budget {} exhausted", budget.fuel)));
                    }
                    if Instant::now() >= budget.deadline {
                        return Err(CallError::Fault("deadline exceeded".to_owned()));
                    }
                    let slice = budget.slice.max(pending.required_fuel());
                    granted = granted.saturating_add(slice);
                    self.store.set_fuel(slice).map_err(|err| CallError::Fault(err.to_string()))?;
                    call = pending.resume(&mut self.store).map_err(|err| CallError::Fault(format!("trap: {err}")))?;
                }
            }
        }
    }

    /// Turn a finished call's return code and host state into output or a guest error.
    fn finish(&mut self, what: &str, code: i32) -> Result<CallOutput, CallError> {
        let state = self.store.data_mut();
        let logs = std::mem::take(&mut state.logs);
        if let Some(text) = state.fail.take() {
            return Err(CallError::Guest { text: format!("{what}{text}"), logs });
        }
        if code != 0 {
            return Err(CallError::Guest { text: format!("{what}guest returned {code}"), logs });
        }
        Ok(CallOutput { outputs: std::mem::take(&mut state.outputs), logs, status: state.status.take() })
    }

    /// Pass the node configuration (one EVE object) to `el_init`, if the guest exports it.
    /// `emit` is refused during init.
    pub(crate) fn init(
        &mut self,
        config: &[u8],
        budget: &Budget,
        cancel: &AtomicBool,
    ) -> Result<CallOutput, CallError> {
        let Some(init) = self.init else {
            return Ok(CallOutput::default());
        };
        self.reset();
        let (ptr, len) = self.write_input(config, budget)?;
        let outputs = std::mem::replace(&mut self.store.data_mut().outputs_allowed, 0);
        self.store.set_fuel(budget.slice).map_err(|err| CallError::Fault(err.to_string()))?;
        let result = init
            .call_resumable(&mut self.store, (ptr, len))
            .map_err(|err| CallError::Fault(format!("el_init: trap: {err}")))
            .and_then(|call| self.drive(call, budget, cancel));
        self.store.data_mut().outputs_allowed = outputs;
        let code = result.map_err(|err| match err {
            CallError::Fault(text) => CallError::Fault(format!("el_init: {text}")),
            other => other,
        })?;
        self.finish("el_init: ", code)
    }

    /// Call `el_close`, if exported, when the node stops. Bounded like any other call.
    pub(crate) fn close(&mut self, budget: &Budget, cancel: &AtomicBool) -> Result<CallOutput, CallError> {
        let Some(close) = self.close else {
            return Ok(CallOutput::default());
        };
        self.reset();
        let outputs = std::mem::replace(&mut self.store.data_mut().outputs_allowed, 0);
        self.store.set_fuel(budget.slice).map_err(|err| CallError::Fault(err.to_string()))?;
        let result = close
            .call_resumable(&mut self.store, ())
            .map_err(|err| CallError::Fault(format!("el_close: trap: {err}")))
            .and_then(|call| self.drive(call, budget, cancel));
        self.store.data_mut().outputs_allowed = outputs;
        result?;
        self.finish("el_close: ", 0)
    }

    /// One message. Outputs are returned only when the guest returns `0` without calling `fail`.
    pub(crate) fn call(&mut self, input: &[u8], budget: &Budget, cancel: &AtomicBool) -> Result<CallOutput, CallError> {
        self.reset();
        let (ptr, len) = self.write_input(input, budget)?;
        self.store.set_fuel(budget.slice).map_err(|err| CallError::Fault(err.to_string()))?;
        let call = self
            .on_input
            .call_resumable(&mut self.store, (ptr, len))
            .map_err(|err| CallError::Fault(format!("trap: {err}")))?;
        let code = self.drive(call, budget, cancel)?;
        self.finish("", code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    fn budget(fuel: u64, ms: u64) -> Budget {
        Budget { fuel, slice: 1_000_000, deadline: Instant::now() + Duration::from_millis(ms) }
    }

    fn instance(wat: &str, pages: u32) -> Instance {
        let wasm = wat::parse_str(wat).unwrap();
        let cell = EngineCell::new().unwrap();
        let module = cell.compile(&wasm).unwrap();
        cell.instantiate(&module, pages, 1, &budget(20_000_000, 1000)).unwrap()
    }

    #[test]
    fn uppercase_guest_emits_ascii() {
        let mut instance = instance(include_str!("fixtures/upper.wat"), 1);
        let out = instance.call(b"AbC", &budget(20_000_000, 1000), &AtomicBool::new(false)).unwrap();
        assert_eq!(out.outputs.len(), 1);
        assert_eq!(out.outputs[0].1, b"ABC");
    }

    #[test]
    fn wasi_import_is_rejected() {
        let wat = r#"(module (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32))) (memory (export "memory") 1) (func (export "el_abi_version") (result i32) i32.const 1) (func (export "el_alloc") (param i32) (result i32) i32.const 0) (func (export "el_on_input") (param i32 i32) (result i32) i32.const 0))"#;
        let cell = EngineCell::new().unwrap();
        let err = cell.compile(&wat::parse_str(wat).unwrap()).unwrap_err();
        assert!(err.to_string().contains("not granted"), "{err}");
    }

    #[test]
    fn unknown_abi_import_name_is_rejected() {
        let wat = r#"(module (import "n2link:node/v1" "clock" (func (result i64))) (memory (export "memory") 1) (func (export "el_abi_version") (result i32) i32.const 1) (func (export "el_alloc") (param i32) (result i32) i32.const 0) (func (export "el_on_input") (param i32 i32) (result i32) i32.const 0))"#;
        let cell = EngineCell::new().unwrap();
        let err = cell.compile(&wat::parse_str(wat).unwrap()).unwrap_err();
        assert!(err.to_string().contains("n2link:node/v1::clock"), "{err}");
    }

    #[test]
    fn an_edgelink_import_gets_a_rebuild_hint() {
        let wat = r#"(module (import "edgelink:node/v1" "emit" (func (param i32 i32 i32) (result i32))) (memory (export "memory") 1) (func (export "el_abi_version") (result i32) i32.const 1) (func (export "el_alloc") (param i32) (result i32) i32.const 0) (func (export "el_on_input") (param i32 i32) (result i32) i32.const 0))"#;
        let cell = EngineCell::new().unwrap();
        let err = cell.compile(&wat::parse_str(wat).unwrap()).unwrap_err();
        assert!(err.to_string().contains("from EdgeLinkd"), "{err}");
    }

    #[test]
    fn float_guest_is_accepted() {
        let wat = r#"(module (memory (export "memory") 1) (func (export "el_abi_version") (result i32) i32.const 1) (func (export "el_alloc") (param i32) (result i32) i32.const 0) (func (export "el_on_input") (param i32 i32) (result i32) (drop (f64.mul (f64.const 1.5) (f64.const 2))) i32.const 0))"#;
        let mut instance = instance(wat, 1);
        instance.call(b"", &budget(20_000_000, 1000), &AtomicBool::new(false)).unwrap();
    }

    #[test]
    fn infinite_loop_exhausts_fuel_then_deadline() {
        let mut spin = instance(include_str!("fixtures/spin.wat"), 1);
        let err = spin.call(b"x", &budget(5_000_000, 5_000), &AtomicBool::new(false)).unwrap_err();
        assert!(err.to_string().contains("fuel budget"), "{err}");
        let started = Instant::now();
        let err = spin.call(b"x", &budget(u64::MAX / 4, 50), &AtomicBool::new(false)).unwrap_err();
        assert!(err.to_string().contains("deadline"), "{err}");
        assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
    }

    #[test]
    fn cancellation_stops_at_the_next_slice() {
        let mut spin = instance(include_str!("fixtures/spin.wat"), 1);
        let err = spin.call(b"x", &budget(u64::MAX / 4, 10_000), &AtomicBool::new(true)).unwrap_err();
        assert!(matches!(err, CallError::Cancelled), "{err}");
    }

    #[test]
    fn memory_growth_is_capped() {
        let mut grow = instance(include_str!("fixtures/grow.wat"), 2);
        let err = grow.call(b"x", &budget(20_000_000, 1000), &AtomicBool::new(false)).unwrap_err();
        assert!(matches!(err, CallError::Fault(_)), "{err}");
    }

    #[test]
    fn oversized_emit_and_bad_port_are_faults() {
        let wat = r#"(module (import "n2link:node/v1" "emit" (func $emit (param i32 i32 i32) (result i32))) (memory (export "memory") 2) (func (export "el_abi_version") (result i32) i32.const 1) (func (export "el_alloc") (param i32) (result i32) i32.const 0) (func (export "el_on_input") (param i32 i32) (result i32) (call $emit (i32.const 0) (i32.const 0) (i32.const 70000))))"#;
        let err = instance(wat, 2).call(b"", &budget(20_000_000, 1000), &AtomicBool::new(false)).unwrap_err();
        assert!(err.to_string().contains("exceeds 65536"), "{err}");
        let wat = r#"(module (import "n2link:node/v1" "emit" (func $emit (param i32 i32 i32) (result i32))) (memory (export "memory") 1) (func (export "el_abi_version") (result i32) i32.const 1) (func (export "el_alloc") (param i32) (result i32) i32.const 0) (func (export "el_on_input") (param i32 i32) (result i32) (call $emit (i32.const 3) (i32.const 0) (i32.const 1))))"#;
        let err = instance(wat, 1).call(b"", &budget(20_000_000, 1000), &AtomicBool::new(false)).unwrap_err();
        assert!(err.to_string().contains("bad output port 3"), "{err}");
    }

    #[test]
    fn log_status_and_fail_are_recorded() {
        let mut guest = instance(include_str!("fixtures/report.wat"), 1);
        let err = guest.call(b"", &budget(20_000_000, 1000), &AtomicBool::new(false)).unwrap_err();
        match err {
            CallError::Guest { text, logs } => {
                assert_eq!(text, "bad row");
                assert_eq!(logs, vec![(GuestLogLevel::Warn, "hello".to_owned())]);
            }
            other => panic!("unexpected {other}"),
        }
    }

    const BASE: &str = r#"(memory (export "memory") 1) (func (export "el_abi_version") (result i32) i32.const 1) (func (export "el_alloc") (param i32) (result i32) i32.const 0)"#;

    fn compile_err(body: &str) -> String {
        let wat = format!("(module {body})");
        EngineCell::new().unwrap().compile(&wat::parse_str(&wat).unwrap()).unwrap_err().to_string()
    }

    #[test]
    fn missing_mistyped_and_imported_abi_items_are_rejected() {
        let on_input = r#"(func (export "el_on_input") (param i32 i32) (result i32) i32.const 0)"#;
        let err = compile_err(BASE);
        assert!(err.contains("does not export el_on_input"), "{err}");
        let err = compile_err(&format!(r#"{BASE} (func (export "el_on_input") (param i32) (result i32) i32.const 0)"#));
        assert!(err.contains("export el_on_input has type"), "{err}");
        let err = compile_err(&format!(r#"{BASE} {on_input} (func (export "el_close") (result i32) i32.const 0)"#));
        assert!(err.contains("export el_close has type"), "{err}");
        let err = compile_err(&format!(
            r#"(func (export "el_abi_version") (result i32) i32.const 1) (func (export "el_alloc") (param i32) (result i32) i32.const 0) {on_input}"#
        ));
        assert!(err.contains("export its linear memory"), "{err}");
        let err = compile_err(&format!(
            r#"(import "n2link:node/v1" "emit" (memory 1)) (func (export "el_abi_version") (result i32) i32.const 1) (func (export "el_alloc") (param i32) (result i32) i32.const 0) {on_input}"#
        ));
        assert!(err.contains("must be a function"), "{err}");
        let err = compile_err(&format!(
            r#"(import "n2link:node/v1" "emit" (func (param i32) (result i32))) {BASE} {on_input}"#
        ));
        assert!(err.contains("import n2link:node/v1::emit has type"), "{err}");
    }

    #[test]
    fn wrong_abi_version_is_not_supported() {
        let wat = r#"(module (memory (export "memory") 1) (func (export "el_abi_version") (result i32) i32.const 2) (func (export "el_alloc") (param i32) (result i32) i32.const 0) (func (export "el_on_input") (param i32 i32) (result i32) i32.const 0))"#;
        let cell = EngineCell::new().unwrap();
        let module = cell.compile(&wat::parse_str(wat).unwrap()).unwrap();
        let err = cell.instantiate(&module, 1, 1, &budget(20_000_000, 1000)).err().unwrap().to_string();
        assert!(err.contains("unsupported WASM ABI 2"), "{err}");
    }

    #[test]
    fn init_receives_config_and_close_is_bounded() {
        let mut guest = instance(include_str!("fixtures/config.wat"), 1);
        guest.init(b"cfg", &budget(20_000_000, 1000), &AtomicBool::new(false)).unwrap();
        let out = guest.call(b"x", &budget(20_000_000, 1000), &AtomicBool::new(false)).unwrap();
        assert_eq!(out.outputs, vec![(0, b"cfg".to_vec())]);
        match guest.close(&budget(20_000_000, 1000), &AtomicBool::new(false)).unwrap_err() {
            CallError::Guest { text, .. } => assert_eq!(text, "el_close: bye"),
            other => panic!("unexpected {other}"),
        }
        let wat = format!(
            r#"(module (import "n2link:node/v1" "emit" (func $emit (param i32 i32 i32) (result i32))) {BASE} (func (export "el_on_input") (param i32 i32) (result i32) i32.const 0) (func (export "el_init") (param i32 i32) (result i32) (call $emit (i32.const 0) (i32.const 0) (i32.const 1))) (func (export "el_close") (loop $l (br $l))))"#
        );
        let mut guest = instance(&wat, 1);
        let err = guest.init(b"{}", &budget(20_000_000, 1000), &AtomicBool::new(false)).unwrap_err();
        assert!(err.to_string().contains("el_init: emit: bad output port 0"), "{err}");
        let err = guest.close(&budget(2_000_000, 1000), &AtomicBool::new(false)).unwrap_err();
        assert!(err.to_string().contains("fuel budget"), "{err}");
    }

    #[test]
    fn start_function_is_rejected() {
        let wat = r#"(module (memory (export "memory") 1) (func $s) (start $s) (func (export "el_abi_version") (result i32) i32.const 1))"#;
        let cell = EngineCell::new().unwrap();
        assert!(cell.compile(&wat::parse_str(wat).unwrap()).is_err());
    }
}
