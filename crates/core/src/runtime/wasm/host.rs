//! Per-engine WASM host state: settings, concurrency permits, the lazily created Wasmi engine,
//! and memory-budget admission for the graph being built.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::EdgelinkError;

use super::exec::{Budget, EngineCell};
use super::plugin_set::PluginSpec;
use super::settings::WasmSettings;

/// Translated code is budgeted at this multiple of the module size (ADR-0002 §10; measured ≈ 6.6×).
const TRANSLATED_CODE_FACTOR: u64 = 8;

/// Per-plugin limits after applying manifest requests and ceilings.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EffectiveLimits {
    pub memory_pages: u32,
    pub fuel: u64,
    pub deadline: Duration,
}

#[derive(Default)]
struct Admission {
    reserved_kib: u64,
    modules: HashSet<[u8; 32]>,
}

pub(crate) struct WasmRuntime {
    settings: WasmSettings,
    permits: Arc<tokio::sync::Semaphore>,
    cell: Mutex<Weak<EngineCell>>,
    admission: Mutex<Admission>,
}

impl WasmRuntime {
    pub(crate) fn new(settings: WasmSettings) -> Self {
        let permits = Arc::new(tokio::sync::Semaphore::new(settings.max_concurrent as usize));
        Self { settings, permits, cell: Mutex::new(Weak::new()), admission: Mutex::new(Admission::default()) }
    }

    pub(crate) fn settings(&self) -> &WasmSettings {
        &self.settings
    }

    pub(crate) fn permits(&self) -> Arc<tokio::sync::Semaphore> {
        self.permits.clone()
    }

    /// The shared Wasmi engine. Created by the first plugin node that runs; dropped when the last
    /// node holding it stops, so a graph without plugin nodes keeps no engine alive.
    pub(crate) fn engine_cell(&self) -> crate::Result<Arc<EngineCell>> {
        let mut slot = self.cell.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cell) = slot.upgrade() {
            return Ok(cell);
        }
        let cell = Arc::new(EngineCell::new()?);
        *slot = Arc::downgrade(&cell);
        Ok(cell)
    }

    /// Whether a Wasmi engine exists right now (some plugin node has run).
    pub(crate) fn engine_live(&self) -> bool {
        self.cell.lock().unwrap_or_else(|e| e.into_inner()).strong_count() > 0
    }

    /// Permits held by running plugin calls.
    pub(crate) fn permits_in_use(&self) -> usize {
        (self.settings.max_concurrent as usize).saturating_sub(self.permits.available_permits())
    }

    /// Memory admitted for the deployed graph, in KiB.
    pub(crate) fn reserved_kib(&self) -> u64 {
        self.admission.lock().unwrap_or_else(|e| e.into_inner()).reserved_kib
    }

    /// The limits a plugin runs with: its manifest requests, or the defaults, never above the
    /// configured ceilings (a request above a ceiling fails the deploy instead of being clamped).
    pub(crate) fn effective_limits(&self, spec: &PluginSpec) -> crate::Result<EffectiveLimits> {
        let s = &self.settings;
        let check = |name: &str, value: u64, max: u64, key: &str| -> crate::Result<u64> {
            if value > max {
                return Err(EdgelinkError::NotSupported(format!(
                    "WASM plugin {} requests limits.{name} = {value}, above [runtime.wasm] {key} = {max}",
                    spec.id
                )));
            }
            Ok(value)
        };
        let pages = spec.limits.memory_pages.unwrap_or(s.default_memory_pages);
        let fuel = spec.limits.fuel_per_message.unwrap_or(s.default_fuel);
        let deadline_ms = spec.limits.deadline_ms.unwrap_or(s.default_deadline_ms);
        Ok(EffectiveLimits {
            memory_pages: check("memory_pages", u64::from(pages), u64::from(s.max_memory_pages), "max_memory_pages")?
                as u32,
            fuel: check("fuel_per_message", fuel, s.max_fuel, "max_fuel")?.max(s.fuel_slice),
            deadline: Duration::from_millis(check("deadline_ms", deadline_ms, s.max_deadline_ms, "max_deadline_ms")?),
        })
    }

    pub(crate) fn budget(&self, limits: &EffectiveLimits) -> Budget {
        Budget { fuel: limits.fuel, slice: self.settings.fuel_slice, deadline: Instant::now() + limits.deadline }
    }

    pub(crate) fn max_input_bytes(&self) -> usize {
        self.settings.max_input_kib as usize * 1024
    }

    /// Reserve one plugin node's footprint for the graph being built.
    pub(crate) fn admit(&self, spec: &PluginSpec, limits: &EffectiveLimits) -> crate::Result<()> {
        let mut admission = self.admission.lock().unwrap_or_else(|e| e.into_inner());
        let mut need = u64::from(limits.memory_pages) * 64;
        if !admission.modules.contains(&spec.sha256) {
            need += (spec.wasm.len() as u64).div_ceil(1024) * TRANSLATED_CODE_FACTOR;
        }
        let budget = u64::from(self.settings.memory_budget_kib);
        if admission.reserved_kib + need > budget {
            return Err(EdgelinkError::NotSupported(format!(
                "WASM memory budget exceeded: node type '{}' needs {need} KiB, {} of {budget} KiB already reserved \
                 ([runtime.wasm] memory_budget_kib)",
                spec.type_name, admission.reserved_kib
            )));
        }
        admission.reserved_kib += need;
        admission.modules.insert(spec.sha256);
        Ok(())
    }

    /// The graph was cleared (redeploy, restore); its reservations go with it.
    pub(crate) fn reset_admission(&self) {
        *self.admission.lock().unwrap_or_else(|e| e.into_inner()) = Admission::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_is_created_lazily_and_dropped_with_its_last_user() {
        let runtime = WasmRuntime::new(WasmSettings::default());
        assert!(!runtime.engine_live());
        let cell = runtime.engine_cell().unwrap();
        let again = runtime.engine_cell().unwrap();
        assert!(Arc::ptr_eq(&cell, &again));
        assert!(runtime.engine_live());
        drop((cell, again));
        assert!(!runtime.engine_live());
    }
}
