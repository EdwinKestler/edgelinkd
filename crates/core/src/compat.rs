//! EdgeLinkd → n2link compatibility for the 0.4.x releases.
//!
//! The old environment variables (`EDGELINK_*`), home directory (`~/.edgelinkd`) and config files
//! (`edgelinkd*.toml`) still work, with a deprecation warning, so an existing installation keeps
//! running after the rename. Remove this module in the next minor release
//! (see `adoption/rebrand/PLAN.md`). Encrypted credentials are a clean break and are not handled here.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Prefix of n2link environment variables.
pub const ENV_PREFIX: &str = "N2LINK_";
/// Prefix accepted with a warning in 0.4.x.
pub const LEGACY_ENV_PREFIX: &str = "EDGELINK_";
/// Default home directory name under the user's home.
pub const HOME_DIR_NAME: &str = ".n2linkd";
/// Home directory name used when only it exists.
pub const LEGACY_HOME_DIR_NAME: &str = ".edgelinkd";
/// Config file stem: `n2linkd.toml`, `n2linkd.<env>.toml`.
pub const CONFIG_STEM: &str = "n2linkd";
/// Config file stem used when only it exists.
pub const LEGACY_CONFIG_STEM: &str = "edgelinkd";

/// Print a deprecation once per message: on stderr (the logger may not be up yet) and to the log.
pub fn deprecated(message: impl Into<String>) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let message = message.into();
    let first = SEEN.get_or_init(|| Mutex::new(HashSet::new())).lock().map(|mut seen| seen.insert(message.clone()));
    if first.unwrap_or(true) {
        eprintln!("warning: {message}");
        log::warn!("{message}");
    }
}

/// `N2LINK_<name>`, falling back to `EDGELINK_<name>` with a warning.
///
/// Both set to different values is an error: n2link will not guess which one is meant.
pub fn env_var_os(name: &str) -> Result<Option<OsString>, String> {
    let key = format!("{ENV_PREFIX}{name}");
    let legacy = format!("{LEGACY_ENV_PREFIX}{name}");
    match (std::env::var_os(&key), std::env::var_os(&legacy)) {
        (Some(value), Some(old)) if value != old => {
            Err(format!("{key} and {legacy} are both set to different values; unset {legacy}"))
        }
        (Some(value), _) => Ok(Some(value)),
        (None, Some(old)) => {
            deprecated(format!("{legacy} is deprecated; rename it to {key}"));
            Ok(Some(old))
        }
        (None, None) => Ok(None),
    }
}

/// [`env_var_os`] as UTF-8.
pub fn env_var(name: &str) -> Result<Option<String>, String> {
    match env_var_os(name)? {
        Some(value) => value.into_string().map(Some).map_err(|_| format!("{ENV_PREFIX}{name} is not UTF-8")),
        None => Ok(None),
    }
}

/// A variable named in configuration (such as `credentials.key_env`). An `N2LINK_*` name also
/// accepts its `EDGELINK_*` counterpart; any other name is read as is.
pub fn named_env_var_os(key: &str) -> Result<Option<OsString>, String> {
    match key.strip_prefix(ENV_PREFIX) {
        Some(name) => env_var_os(name),
        None => Ok(std::env::var_os(key)),
    }
}

/// The default home directory under `user_home`: `.n2linkd`, or `.edgelinkd` when only that one
/// exists. The old directory is never moved automatically: it holds credentials and keys.
pub fn default_home_dir(user_home: &Path) -> PathBuf {
    let home = user_home.join(HOME_DIR_NAME);
    let legacy = user_home.join(LEGACY_HOME_DIR_NAME);
    if !home.exists() && legacy.is_dir() {
        deprecated(format!(
            "using the legacy home directory {}; move it with: mv {} {}",
            legacy.display(),
            legacy.display(),
            home.display()
        ));
        return legacy;
    }
    home
}

/// `n2linkd.toml` (no `env`) or `n2linkd.<env>.toml` in `dir`; the `edgelinkd` file when only it
/// exists. Never both: the new name wins as soon as it exists.
pub fn config_file(dir: &Path, env: Option<&str>) -> PathBuf {
    let name = |stem: &str| match env {
        Some(env) => format!("{stem}.{env}.toml"),
        None => format!("{stem}.toml"),
    };
    let file = dir.join(name(CONFIG_STEM));
    let legacy = dir.join(name(LEGACY_CONFIG_STEM));
    if !file.exists() && legacy.is_file() {
        deprecated(format!("using the legacy config file {}; rename it to {}", legacy.display(), file.display()));
        return legacy;
    }
    file
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test owns distinct variable names, so parallel tests do not race on the environment.
    fn set(key: &str, value: &str) {
        // SAFETY: test-only, and no other test reads or writes these names.
        unsafe { std::env::set_var(key, value) }
    }

    #[test]
    fn the_new_variable_wins_and_the_old_one_still_works() {
        set("N2LINK_COMPAT_T1", "new");
        assert_eq!(env_var("COMPAT_T1").unwrap().as_deref(), Some("new"));
        set("EDGELINK_COMPAT_T2", "old");
        assert_eq!(env_var("COMPAT_T2").unwrap().as_deref(), Some("old"));
        set("N2LINK_COMPAT_T3", "same");
        set("EDGELINK_COMPAT_T3", "same");
        assert_eq!(env_var("COMPAT_T3").unwrap().as_deref(), Some("same"));
        assert_eq!(env_var("COMPAT_T_UNSET").unwrap(), None);
    }

    #[test]
    fn conflicting_values_fail_loudly() {
        set("N2LINK_COMPAT_T4", "a");
        set("EDGELINK_COMPAT_T4", "b");
        let err = env_var("COMPAT_T4").unwrap_err();
        assert!(err.contains("EDGELINK_COMPAT_T4"), "{err}");
    }

    #[test]
    fn configured_variable_names_follow_the_same_rule() {
        set("EDGELINK_COMPAT_T5", "old");
        assert_eq!(named_env_var_os("N2LINK_COMPAT_T5").unwrap(), Some(OsString::from("old")));
        set("CUSTOM_COMPAT_T6", "x");
        assert_eq!(named_env_var_os("CUSTOM_COMPAT_T6").unwrap(), Some(OsString::from("x")));
        set("EDGELINK_COMPAT_T7", "ignored");
        assert_eq!(named_env_var_os("OTHER_COMPAT_T7").unwrap(), None);
    }

    #[test]
    fn legacy_home_and_config_files_are_used_only_when_alone() {
        let root = std::env::temp_dir().join(format!("n2link-compat-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(default_home_dir(&root), root.join(".n2linkd"));
        std::fs::create_dir(root.join(".edgelinkd")).unwrap();
        assert_eq!(default_home_dir(&root), root.join(".edgelinkd"));
        std::fs::create_dir(root.join(".n2linkd")).unwrap();
        assert_eq!(default_home_dir(&root), root.join(".n2linkd"));

        assert_eq!(config_file(&root, None), root.join("n2linkd.toml"));
        std::fs::write(root.join("edgelinkd.toml"), "").unwrap();
        std::fs::write(root.join("edgelinkd.dev.toml"), "").unwrap();
        assert_eq!(config_file(&root, None), root.join("edgelinkd.toml"));
        assert_eq!(config_file(&root, Some("dev")), root.join("edgelinkd.dev.toml"));
        std::fs::write(root.join("n2linkd.toml"), "").unwrap();
        assert_eq!(config_file(&root, None), root.join("n2linkd.toml"));
        assert_eq!(config_file(&root, Some("prod")), root.join("n2linkd.prod.toml"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
