//! `[runtime.wasm]` settings. `enabled` defaults to `false`, even with `nodes_wasm` compiled in.

use crate::EdgelinkError;

/// Keys this prototype implements.
const KNOWN_KEYS: &[&str] = &[
    "enabled",
    "dir",
    "max_plugins",
    "max_module_kib",
    "max_concurrent",
    "memory_budget_kib",
    "default_memory_pages",
    "max_memory_pages",
    "default_fuel",
    "max_fuel",
    "fuel_slice",
    "default_deadline_ms",
    "max_deadline_ms",
    "max_input_kib",
    "failure_threshold",
    "failure_window_s",
    "require_signature",
];

#[derive(Debug, Clone)]
pub(crate) struct WasmSettings {
    pub enabled: bool,
    /// Plugin store directory; relative paths are under `home_dir`.
    pub dir: String,
    pub max_plugins: u32,
    pub max_module_kib: u32,
    pub max_concurrent: u32,
    pub memory_budget_kib: u32,
    pub default_memory_pages: u32,
    pub max_memory_pages: u32,
    pub default_fuel: u64,
    pub max_fuel: u64,
    pub fuel_slice: u64,
    pub default_deadline_ms: u64,
    pub max_deadline_ms: u64,
    pub max_input_kib: u32,
    pub failure_threshold: u32,
    pub failure_window_s: u64,
}

impl Default for WasmSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: "plugins".to_owned(),
            max_plugins: 16,
            max_module_kib: 512,
            max_concurrent: 2,
            memory_budget_kib: 8192,
            // 512 KiB: G1 measured ≈ 82 KiB per instance, and the default memory budget then
            // admits about 15 plugin nodes. Plugins that need more request [limits] memory_pages.
            default_memory_pages: 8,
            max_memory_pages: 256,
            default_fuel: 20_000_000,
            max_fuel: 1_000_000_000,
            fuel_slice: 1_000_000,
            default_deadline_ms: 250,
            max_deadline_ms: 5000,
            max_input_kib: 64,
            failure_threshold: 3,
            failure_window_s: 60,
        }
    }
}

/// `runtime.wasm.enabled` as a strict boolean: absent → `None`, wrong type → error.
pub(crate) fn enabled_flag(cfg: &config::Config) -> crate::Result<Option<bool>> {
    super::strict_bool(cfg, "runtime.wasm.enabled")
}

impl WasmSettings {
    pub(crate) fn from_config(cfg: Option<&config::Config>) -> crate::Result<Self> {
        let mut settings = Self::default();
        let Some(cfg) = cfg else {
            return Ok(settings);
        };
        if let Ok(table) = cfg.get_table("runtime.wasm") {
            for key in table.keys() {
                if !KNOWN_KEYS.contains(&key.as_str()) {
                    return Err(named(&format!("runtime.wasm.{key}"), "is not a known setting"));
                }
            }
        }
        if let Some(enabled) = enabled_flag(cfg)? {
            settings.enabled = enabled;
        }
        if cfg.get::<config::Value>("runtime.wasm.dir").is_ok() {
            let dir = cfg.get_string("runtime.wasm.dir").map_err(|_| named("runtime.wasm.dir", "must be a string"))?;
            if dir.trim().is_empty() {
                return Err(named("runtime.wasm.dir", "must not be empty"));
            }
            settings.dir = dir;
        }
        assign_u32(cfg, "runtime.wasm.max_plugins", 1, 256, &mut settings.max_plugins)?;
        assign_u32(cfg, "runtime.wasm.max_module_kib", 1, 4096, &mut settings.max_module_kib)?;
        assign_u32(cfg, "runtime.wasm.max_concurrent", 1, 64, &mut settings.max_concurrent)?;
        assign_u32(cfg, "runtime.wasm.memory_budget_kib", 64, 1_048_576, &mut settings.memory_budget_kib)?;
        assign_u32(cfg, "runtime.wasm.default_memory_pages", 1, 1024, &mut settings.default_memory_pages)?;
        assign_u32(cfg, "runtime.wasm.max_memory_pages", 1, 4096, &mut settings.max_memory_pages)?;
        assign_u64(cfg, "runtime.wasm.default_fuel", 1, u64::MAX / 4, &mut settings.default_fuel)?;
        assign_u64(cfg, "runtime.wasm.max_fuel", 1, u64::MAX / 4, &mut settings.max_fuel)?;
        assign_u64(cfg, "runtime.wasm.fuel_slice", 1, u64::MAX / 4, &mut settings.fuel_slice)?;
        assign_u64(cfg, "runtime.wasm.default_deadline_ms", 1, 60_000, &mut settings.default_deadline_ms)?;
        assign_u64(cfg, "runtime.wasm.max_deadline_ms", 1, 60_000, &mut settings.max_deadline_ms)?;
        assign_u32(cfg, "runtime.wasm.max_input_kib", 1, 1024, &mut settings.max_input_kib)?;
        assign_u32(cfg, "runtime.wasm.failure_threshold", 1, 100, &mut settings.failure_threshold)?;
        assign_u64(cfg, "runtime.wasm.failure_window_s", 1, 3600, &mut settings.failure_window_s)?;
        if settings.default_memory_pages > settings.max_memory_pages {
            return Err(named("runtime.wasm.default_memory_pages", "must be <= max_memory_pages"));
        }
        if settings.default_fuel > settings.max_fuel {
            return Err(named("runtime.wasm.default_fuel", "must be <= max_fuel"));
        }
        if settings.fuel_slice > settings.default_fuel {
            return Err(named("runtime.wasm.fuel_slice", "must be <= default_fuel"));
        }
        if settings.default_deadline_ms > settings.max_deadline_ms {
            return Err(named("runtime.wasm.default_deadline_ms", "must be <= max_deadline_ms"));
        }
        if super::strict_bool(cfg, "runtime.wasm.require_signature")? == Some(true) {
            return Err(EdgelinkError::NotSupported(
                "runtime.wasm.require_signature is not implemented in ABI v1".to_owned(),
            ));
        }
        Ok(settings)
    }
}

pub(crate) fn disabled_plugin_error(type_name: &str) -> EdgelinkError {
    EdgelinkError::NotSupported(format!(
        "node type '{type_name}' needs WASM plugins, which are disabled by configuration ([runtime.wasm] enabled = false)"
    ))
}

fn named(key: &str, why: &str) -> EdgelinkError {
    EdgelinkError::invalid_operation(&format!("{key} {why}"))
}

fn assign_u32(cfg: &config::Config, key: &str, min: u32, max: u32, dest: &mut u32) -> crate::Result<()> {
    let mut value = u64::from(*dest);
    assign_u64(cfg, key, u64::from(min), u64::from(max), &mut value)?;
    *dest = value as u32;
    Ok(())
}

fn assign_u64(cfg: &config::Config, key: &str, min: u64, max: u64, dest: &mut u64) -> crate::Result<()> {
    if cfg.get::<config::Value>(key).is_err() {
        return Ok(());
    }
    let value = cfg.get_int(key).map_err(|_| named(key, "must be an integer"))?;
    if value < 0 || (value as u64) < min || (value as u64) > max {
        return Err(named(key, &format!("is out of range {min}..={max}")));
    }
    *dest = value as u64;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(toml: &str) -> config::Config {
        config::Config::builder().add_source(config::File::from_str(toml, config::FileFormat::Toml)).build().unwrap()
    }

    #[test]
    fn defaults_keep_plugins_off() {
        assert!(!WasmSettings::from_config(None).unwrap().enabled);
        assert!(!WasmSettings::from_config(Some(&cfg("[runtime.wasm]\nmax_concurrent = 1"))).unwrap().enabled);
    }

    #[test]
    fn invalid_values_fail_loudly() {
        for (toml, needle) in [
            ("[runtime.wasm]\nenabled = \"yes\"", "must be true or false"),
            ("[runtime.wasm]\nenable = true", "not a known setting"),
            ("[runtime.wasm]\ndir = \"\"", "must not be empty"),
            ("[runtime.wasm]\nmax_concurrent = 0", "out of range"),
            ("[runtime.wasm]\ndefault_fuel = \"lots\"", "must be an integer"),
            ("[runtime.wasm]\nfuel_slice = 30000000", "fuel_slice"),
            ("[runtime.wasm]\nrequire_signature = true", "not implemented"),
        ] {
            let err = WasmSettings::from_config(Some(&cfg(toml))).unwrap_err().to_string();
            assert!(err.contains(needle), "{toml}: {err}");
        }
    }
}
