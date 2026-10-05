//! Phase 7 measurement spike for the WASM node ADR.
//!
//! One binary per runtime feature (`none`, `wasmi`, `wt36`, `wt36-pulley`, `wt36-runtime-only`,
//! `wt48`). Every mode prints one JSON object per line so `run.sh` can tabulate the results.
//!
//! Modes:
//! - `idle`                      engine constructed (nothing compiled), then idle RSS
//! - `load <guest-dir> <n>`      compile `upper.wasm` + `bulk.wasm`, instantiate `n` stores, call once
//! - `bench <guest-dir> <m>`     `m` calls of a 1 KiB message through one instance
//! - `hostile <guest-dir>`       spin, grow, oversized output, WASI, wasm-bindgen, start-fn guests
//! - `precompile <guest-dir>`    (wt36 only) writes `*.cwasm` next to the `.wasm` files
//!
//! This is evidence tooling, not product code: limits are constants, not configuration.

use std::time::Duration;
use std::{env, fs, process::ExitCode};
#[cfg(not(feature = "none"))]
use std::{path::Path, time::Instant};

/// Per-instance linear-memory ceiling used by the spike (16 Wasm pages).
pub const MEM_LIMIT: usize = 1024 * 1024;
/// Fuel budget for one message; `SPIKE_FUEL_PER_CALL` overrides it so the wall-clock
/// deadline path can be exercised separately from fuel exhaustion.
pub fn fuel_per_call() -> u64 {
    env::var("SPIKE_FUEL_PER_CALL").ok().and_then(|v| v.parse().ok()).unwrap_or(50_000_000)
}
/// Fuel slice between cancellation/deadline checks (wasmi resumable calls).
pub const FUEL_SLICE: u64 = 1_000_000;
/// Wall-clock deadline for one message.
pub const DEADLINE: Duration = Duration::from_millis(100);
/// Largest single `emit` payload the host accepts.
pub const MAX_EMIT: usize = 64 * 1024;
/// Largest number of `emit` calls per message.
pub const MAX_EMITS: usize = 16;

#[cfg(feature = "wasmi")]
#[path = "rt_wasmi.rs"]
mod rt;

#[cfg(any(feature = "wt36", feature = "wt36-runtime-only", feature = "wt48"))]
#[path = "rt_wasmtime.rs"]
mod rt;

#[cfg(feature = "none")]
mod rt {
    pub const NAME: &str = "none";
    pub struct Engine;
    impl Engine {
        pub fn new() -> Result<Self, String> {
            Ok(Engine)
        }
    }
}

fn status_kib(key: &str) -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn rss_line(mode: &str, extra: &str) {
    println!(
        "{{\"rt\":\"{}\",\"mode\":\"{mode}\",\"rss_kib\":{},\"hwm_kib\":{}{extra}}}",
        rt::NAME,
        status_kib("VmRSS:"),
        status_kib("VmHWM:")
    );
}

#[cfg(not(feature = "none"))]
fn read_guest(dir: &Path, name: &str) -> Vec<u8> {
    #[cfg(feature = "wt36-runtime-only")]
    let file = dir.join(format!("{name}.cwasm"));
    #[cfg(not(feature = "wt36-runtime-only"))]
    let file = dir.join(format!("{name}.wasm"));
    fs::read(&file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()))
}

#[cfg(not(feature = "none"))]
fn percentile(sorted: &[u128], p: f64) -> u128 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("idle");
    match run(mode, &args[2..]) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}: {mode}: {e}", rt::NAME);
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "none")]
fn run(mode: &str, _args: &[String]) -> Result<(), String> {
    let _engine = rt::Engine::new()?;
    std::thread::sleep(Duration::from_secs(1));
    rss_line(mode, "");
    Ok(())
}

#[cfg(not(feature = "none"))]
fn run(mode: &str, args: &[String]) -> Result<(), String> {
    let engine = rt::Engine::new()?;
    match mode {
        "idle" => {
            std::thread::sleep(Duration::from_secs(1));
            rss_line(mode, "");
        }
        "load" => {
            let dir = Path::new(&args[0]);
            let n: usize = args[1].parse().map_err(|e| format!("n: {e}"))?;
            let before = status_kib("VmRSS:");
            let t = Instant::now();
            let upper = engine.compile(&read_guest(dir, "upper"))?;
            let compile_upper_us = t.elapsed().as_micros();
            let t = Instant::now();
            let bulk = engine.compile(&read_guest(dir, "bulk"))?;
            let compile_bulk_us = t.elapsed().as_micros();
            let after_compile = status_kib("VmRSS:");
            let t = Instant::now();
            let mut instances = Vec::with_capacity(n);
            for _ in 0..n {
                instances.push(engine.instantiate(&upper)?);
            }
            let inst_us = t.elapsed().as_micros() / n.max(1) as u128;
            let msg = vec![b'a'; 1024];
            for inst in &mut instances {
                let out = inst.on_input(&msg)?;
                if out.len() != 1 || out[0].len() != 1024 || out[0][0] != b'A' {
                    return Err("unexpected output".into());
                }
            }
            let mut bulk_inst = engine.instantiate(&bulk)?;
            bulk_inst.on_input(&msg)?;
            std::thread::sleep(Duration::from_secs(1));
            let after = status_kib("VmRSS:");
            rss_line(
                mode,
                &format!(
                    ",\"n\":{n},\"rss_before_kib\":{before},\"rss_after_compile_kib\":{after_compile},\
                     \"per_instance_kib\":{},\"compile_upper_us\":{compile_upper_us},\
                     \"compile_bulk_us\":{compile_bulk_us},\"instantiate_us\":{inst_us}",
                    after.saturating_sub(after_compile) / n.max(1) as u64
                ),
            );
            drop(instances);
            drop(bulk_inst);
        }
        "bench" => {
            let dir = Path::new(&args[0]);
            let m: usize = args[1].parse().map_err(|e| format!("m: {e}"))?;
            let module = engine.compile(&read_guest(dir, "upper"))?;
            let mut inst = engine.instantiate(&module)?;
            let msg = vec![b'a'; 1024];
            let mut samples = Vec::with_capacity(m);
            for _ in 0..m {
                let t = Instant::now();
                inst.on_input(&msg)?;
                samples.push(t.elapsed().as_nanos());
            }
            samples.sort_unstable();
            rss_line(
                mode,
                &format!(
                    ",\"calls\":{m},\"median_ns\":{},\"p95_ns\":{}",
                    percentile(&samples, 0.5),
                    percentile(&samples, 0.95)
                ),
            );
        }
        "hostile" => {
            let dir = Path::new(&args[0]);
            for name in ["spin", "grow", "bigout", "wasi", "bindgen", "startspin"] {
                let t = Instant::now();
                let outcome = engine
                    .compile(&read_guest(dir, name))
                    .and_then(|m| engine.instantiate(&m))
                    .and_then(|mut i| i.on_input(b"x"));
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                let (ok, text) = match outcome {
                    Ok(_) => (true, "completed".to_string()),
                    Err(e) => (false, e),
                };
                let text = text.replace('\\', "/").replace('"', "'").replace('\n', " ");
                println!(
                    "{{\"rt\":\"{}\",\"mode\":\"hostile\",\"guest\":\"{name}\",\"contained\":{},\
                     \"elapsed_ms\":{ms:.1},\"error\":\"{text}\"}}",
                    rt::NAME,
                    !ok
                );
            }
        }
        #[cfg(feature = "wt36")]
        "precompile" => {
            let dir = Path::new(&args[0]);
            for name in ["upper", "bulk", "spin", "grow", "bigout", "wasi", "bindgen", "startspin"] {
                let cwasm = engine.precompile(&read_guest(dir, name))?;
                fs::write(dir.join(format!("{name}.cwasm")), cwasm).map_err(|e| e.to_string())?;
            }
        }
        other => return Err(format!("unknown mode {other}")),
    }
    Ok(())
}
