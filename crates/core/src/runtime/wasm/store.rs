//! Plugin store and lifecycle (DESIGN.md §9): stage → validate → quarantine → self-test →
//! activate, one previous generation, rollback, removal, and crash recovery on open.
//!
//! Layout under `<home_dir>/<dir>` (directories 0700, files 0600; symlinks are refused):
//!
//! ```text
//! .lock                     held exclusively by whoever has the store open
//! staging/<uuid>.part       upload in progress (deleted on open)
//! quarantine/<sha>.wasm     validated package, never executed except by self-test
//! quarantine/<sha>.json     stage report
//! store/<sha>.wasm|.json    generations referenced by active.toml
//! active.toml               the only pointer to what runs
//! active.toml.prev          pointer before the last change
//! ```

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::N2linkError;
use crate::runtime::model::Variant;

use super::convert::{decode_variant, encode_variant};
use super::exec::{CallError, EngineCell};
use super::host::WasmRuntime;
use super::manifest::{LimitRequest, Manifest, split_id};
use super::plugin_set::{ActivePlugins, PluginSpec};
use super::section::manifest_text;
use super::settings::WasmSettings;

const ACTIVE: &str = "active.toml";
const ACTIVE_PREV: &str = "active.toml.prev";
const ACTIVE_TMP: &str = "active.toml.tmp";
const MAX_QUARANTINED: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageStatus {
    Ready,
    Rejected,
}

/// What `stage` learned about a package. Stored next to it as `<sha>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageReport {
    pub sha256: String,
    pub id: String,
    pub version: String,
    pub license: String,
    pub description: String,
    pub bytes: u64,
    pub outputs: u8,
    pub limits: LimitRequest,
    pub selftests: usize,
    pub status: PackageStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub staged_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveEntry {
    pub current: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    pub activated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ActiveFile {
    schema: u32,
    #[serde(default)]
    plugins: BTreeMap<String, ActiveEntry>,
}

#[derive(Debug, Default, Serialize)]
pub struct Listing {
    pub active: BTreeMap<String, ActiveEntry>,
    pub packages: Vec<StageReport>,
}

/// Checks the candidate plugin set against the deployed graph (`Engine::prepare_flows`).
pub type PrepareFn<'a> = &'a dyn Fn(Arc<ActivePlugins>) -> crate::Result<()>;

/// A pointer change that is written but not yet final. [`PluginStore::finish`] deletes the
/// generation it superseded; [`PluginStore::revert`] restores the pointer it replaced. The
/// online path holds one across the redeploy so a failed redeploy can undo the activation.
#[derive(Debug)]
#[must_use = "finish or revert the pointer change"]
pub struct PendingChange {
    before: ActiveFile,
}

pub struct PluginStore {
    root: PathBuf,
    settings: WasmSettings,
    _lock: File,
    #[cfg(test)]
    fail_at: std::sync::Mutex<Option<&'static str>>,
}

impl std::fmt::Debug for PluginStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginStore").field("root", &self.root).finish()
    }
}

fn io_err(path: &Path, err: std::io::Error) -> N2linkError {
    N2linkError::invalid_operation(&format!("plugin store {}: {err}", path.display()))
}

fn store_err(text: impl Into<String>) -> N2linkError {
    N2linkError::InvalidOperation(text.into())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn valid_sha(sha: &str) -> crate::Result<()> {
    if sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
        Ok(())
    } else {
        Err(store_err(format!("'{sha}' is not a lowercase hex SHA-256")))
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn refuse_symlink(path: &Path) -> crate::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(store_err(format!("plugin store refuses symlink {}", path.display())))
        }
        _ => Ok(()),
    }
}

fn ensure_dir(path: &Path) -> crate::Result<()> {
    refuse_symlink(path)?;
    if !path.exists() {
        fs::create_dir_all(path).map_err(|e| io_err(path, e))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| io_err(path, e))?;
    }
    Ok(())
}

fn open_options() -> OpenOptions {
    #[allow(unused_mut)]
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    options
}

fn read(path: &Path) -> crate::Result<Vec<u8>> {
    refuse_symlink(path)?;
    let mut file = open_options().read(true).open(path).map_err(|e| io_err(path, e))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|e| io_err(path, e))?;
    Ok(bytes)
}

fn sync_dir(dir: &Path) -> crate::Result<()> {
    #[cfg(unix)]
    File::open(dir).and_then(|d| d.sync_all()).map_err(|e| io_err(dir, e))?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Write `bytes` to `path` through a temporary file, fsync, rename, fsync the directory.
fn write_atomic(path: &Path, bytes: &[u8]) -> crate::Result<()> {
    let dir = path.parent().expect("store paths have a parent");
    let tmp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut file = open_options().write(true).create_new(true).open(&tmp).map_err(|e| io_err(&tmp, e))?;
    file.write_all(bytes).and_then(|_| file.sync_all()).map_err(|e| io_err(&tmp, e))?;
    drop(file);
    refuse_symlink(path)?;
    fs::rename(&tmp, path).map_err(|e| io_err(path, e))?;
    sync_dir(dir)
}

fn rename(from: &Path, to: &Path) -> crate::Result<()> {
    refuse_symlink(from)?;
    refuse_symlink(to)?;
    fs::rename(from, to).map_err(|e| io_err(from, e))?;
    sync_dir(to.parent().expect("store paths have a parent"))
}

impl PluginStore {
    /// Open the store named by `[runtime.wasm] dir` under `home_dir`, take its lock and recover.
    pub fn open(cfg: &config::Config) -> crate::Result<Self> {
        let settings = WasmSettings::from_config(Some(cfg))?;
        let dir = PathBuf::from(&settings.dir);
        let root = if dir.is_absolute() {
            dir
        } else {
            let home = cfg
                .get_string("home_dir")
                .map_err(|_| N2linkError::invalid_operation("home_dir is not set; cannot locate the plugin store"))?;
            PathBuf::from(home).join(dir)
        };
        Self::open_at(root, settings)
    }

    pub(crate) fn open_at(root: PathBuf, settings: WasmSettings) -> crate::Result<Self> {
        ensure_dir(&root)?;
        for sub in ["staging", "quarantine", "store"] {
            ensure_dir(&root.join(sub))?;
        }
        let lock_path = root.join(".lock");
        refuse_symlink(&lock_path)?;
        let lock = open_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| io_err(&lock_path, e))?;
        fs2::FileExt::try_lock_exclusive(&lock).map_err(|_| {
            N2linkError::invalid_operation(&format!(
                "plugin store {} is in use by another n2linkd process; stop it or use the admin API",
                root.display()
            ))
        })?;
        let store = Self {
            root,
            settings,
            _lock: lock,
            #[cfg(test)]
            fail_at: std::sync::Mutex::new(None),
        };
        store.recover()?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Largest package `stage` accepts (`[runtime.wasm] max_module_kib`).
    pub fn max_package_bytes(&self) -> usize {
        self.settings.max_module_kib as usize * 1024
    }

    #[cfg(test)]
    fn fail_at(&self, step: &'static str) {
        *self.fail_at.lock().unwrap() = Some(step);
    }

    /// Failure injection point for crash tests; a no-op outside tests.
    fn step(&self, _name: &'static str) -> crate::Result<()> {
        #[cfg(test)]
        if *self.fail_at.lock().unwrap() == Some(_name) {
            return Err(N2linkError::invalid_operation(&format!("injected failure at {_name}")));
        }
        Ok(())
    }

    fn path(&self, area: &str, sha: &str, ext: &str) -> PathBuf {
        self.root.join(area).join(format!("{sha}.{ext}"))
    }

    fn read_active(&self) -> crate::Result<ActiveFile> {
        let path = self.root.join(ACTIVE);
        if !path.exists() {
            return Ok(ActiveFile { schema: 1, plugins: BTreeMap::new() });
        }
        let text =
            String::from_utf8(read(&path)?).map_err(|_| store_err(format!("{} is not UTF-8", path.display())))?;
        let file: ActiveFile = toml_edit::de::from_str(&text).map_err(|err| {
            store_err(format!(
                "{} is corrupt ({err}); restore {ACTIVE_PREV} or run `n2linkd plugin verify`",
                path.display()
            ))
        })?;
        if file.schema != 1 {
            return Err(store_err(format!("{} has unsupported schema {}", path.display(), file.schema)));
        }
        Ok(file)
    }

    /// `active.toml` → `.prev` (copy), new content → `.tmp` → rename. A crash leaves either the
    /// old or the new pointer, never a partial one.
    fn write_active(&self, file: &ActiveFile) -> crate::Result<()> {
        let path = self.root.join(ACTIVE);
        if path.exists() {
            write_atomic(&self.root.join(ACTIVE_PREV), &read(&path)?)?;
        }
        self.step("pointer-prev-written")?;
        let text = toml_edit::ser::to_string_pretty(file)
            .map_err(|err| N2linkError::invalid_operation(&format!("active.toml: {err}")))?;
        let tmp = self.root.join(ACTIVE_TMP);
        let mut out = open_options().write(true).create(true).truncate(true).open(&tmp).map_err(|e| io_err(&tmp, e))?;
        out.write_all(text.as_bytes()).and_then(|_| out.sync_all()).map_err(|e| io_err(&tmp, e))?;
        drop(out);
        self.step("pointer-tmp-written")?;
        rename(&tmp, &path)
    }

    fn referenced(active: &ActiveFile) -> std::collections::BTreeSet<String> {
        active.plugins.values().flat_map(|e| std::iter::once(e.current.clone()).chain(e.previous.clone())).collect()
    }

    /// Crash recovery (DESIGN.md §9 table). Never activates anything.
    fn recover(&self) -> crate::Result<()> {
        for area in ["staging"] {
            for entry in fs::read_dir(self.root.join(area)).map_err(|e| io_err(&self.root, e))? {
                let path = entry.map_err(|e| io_err(&self.root, e))?.path();
                fs::remove_file(&path).map_err(|e| io_err(&path, e))?;
            }
        }
        let tmp = self.root.join(ACTIVE_TMP);
        if tmp.exists() {
            fs::remove_file(&tmp).map_err(|e| io_err(&tmp, e))?;
        }
        let active = self.read_active()?;
        let referenced = Self::referenced(&active);
        // Store files no pointer references (crash between promote and pointer) go back to quarantine.
        for entry in fs::read_dir(self.root.join("store")).map_err(|e| io_err(&self.root, e))? {
            let path = entry.map_err(|e| io_err(&self.root, e))?.path();
            let Some(sha) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else { continue };
            if !referenced.contains(&sha) {
                let target = self.root.join("quarantine").join(path.file_name().expect("file name"));
                rename(&path, &target)?;
            }
        }
        // A quarantined module without its report is an interrupted stage.
        for entry in fs::read_dir(self.root.join("quarantine")).map_err(|e| io_err(&self.root, e))? {
            let path = entry.map_err(|e| io_err(&self.root, e))?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("wasm") && !path.with_extension("json").exists() {
                fs::remove_file(&path).map_err(|e| io_err(&path, e))?;
            }
        }
        Ok(())
    }

    fn report(&self, area: &str, sha: &str) -> crate::Result<Option<StageReport>> {
        let path = self.path(area, sha, "json");
        if !path.exists() {
            return Ok(None);
        }
        let report = serde_json::from_slice(&read(&path)?)
            .map_err(|err| store_err(format!("{} is corrupt: {err}", path.display())))?;
        Ok(Some(report))
    }

    fn quarantined(&self) -> crate::Result<Vec<StageReport>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join("quarantine")).map_err(|e| io_err(&self.root, e))? {
            let path = entry.map_err(|e| io_err(&self.root, e))?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                let sha = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_owned();
                if let Some(report) = self.report("quarantine", &sha)? {
                    out.push(report);
                }
            }
        }
        out.sort_by(|a, b| a.staged_at.cmp(&b.staged_at).then(a.sha256.cmp(&b.sha256)));
        Ok(out)
    }

    /// Validate a package, quarantine it and run its self-tests. A failed validation leaves
    /// nothing behind; a failed self-test leaves a `rejected` report for inspection.
    pub fn stage(&self, bytes: &[u8]) -> crate::Result<StageReport> {
        let sha = hex(&Sha256::digest(bytes));
        for area in ["store", "quarantine"] {
            if let Some(report) = self.report(area, &sha)? {
                return Ok(report);
            }
        }
        if self.quarantined()?.len() >= MAX_QUARANTINED {
            return Err(store_err(format!(
                "{MAX_QUARANTINED} packages are already quarantined; discard one first (`n2linkd plugin discard <sha256>`)"
            )));
        }
        let part = self.root.join("staging").join(format!("{}.part", uuid::Uuid::new_v4()));
        let mut file = open_options().write(true).create_new(true).open(&part).map_err(|e| io_err(&part, e))?;
        file.write_all(bytes).and_then(|_| file.sync_all()).map_err(|e| io_err(&part, e))?;
        drop(file);
        let validated = self.validate(bytes);
        let (manifest, spec) = match validated {
            Ok(ok) => ok,
            Err(err) => {
                let _ = fs::remove_file(&part);
                return Err(err);
            }
        };
        self.step("staged")?;
        rename(&part, &self.path("quarantine", &sha, "wasm"))?;
        self.step("quarantined")?;
        let mut report = StageReport {
            sha256: sha.clone(),
            id: manifest.plugin.id.clone(),
            version: manifest.plugin.version.clone(),
            license: manifest.plugin.license.clone(),
            description: manifest.plugin.description.clone(),
            bytes: bytes.len() as u64,
            outputs: manifest.node.outputs,
            limits: manifest.limits,
            selftests: manifest.selftest.len(),
            status: PackageStatus::Ready,
            reason: None,
            staged_at: now(),
        };
        if let Err(err) = self.self_test(&spec, &manifest) {
            report.status = PackageStatus::Rejected;
            report.reason = Some(err.to_string());
        }
        let json = serde_json::to_vec_pretty(&report).map_err(|e| N2linkError::invalid_operation(&e.to_string()))?;
        write_atomic(&self.path("quarantine", &sha, "json"), &json)?;
        Ok(report)
    }

    /// Framing, manifest, limits against ceilings, Wasm features and imports.
    fn validate(&self, bytes: &[u8]) -> crate::Result<(Manifest, PluginSpec)> {
        let text = manifest_text(bytes, self.settings.max_module_kib as usize * 1024)?;
        let manifest = Manifest::parse(&text)?;
        let spec = PluginSpec::from_package(bytes.to_vec(), &manifest)?;
        let limits = WasmRuntime::new(self.settings.clone()).effective_limits(&spec)?;
        let module = EngineCell::new()?.compile(bytes)?;
        let initial = EngineCell::min_memory_pages(&module);
        if initial > u64::from(limits.memory_pages) {
            return Err(N2linkError::NotSupported(format!(
                "module starts with {initial} pages of linear memory but plugin {} may use {} \
                 ([limits] memory_pages, else [runtime.wasm] default_memory_pages); request \
                 memory_pages = {initial} in the manifest, or link a Rust guest with \
                 -C link-arg=-zstack-size=65536",
                spec.id, limits.memory_pages
            )));
        }
        if !manifest.node.config.is_empty() && !EngineCell::exports(&module, "el_init") {
            return Err(N2linkError::NotSupported(
                "manifest declares [[node.config]] but the module does not export el_init".to_owned(),
            ));
        }
        Ok((manifest, spec))
    }

    /// Instantiate under the plugin's own limits and run every `[[selftest]]` vector.
    fn self_test(&self, spec: &PluginSpec, manifest: &Manifest) -> crate::Result<()> {
        let runtime = WasmRuntime::new(self.settings.clone());
        let limits = runtime.effective_limits(spec)?;
        let cell = EngineCell::new()?;
        let module = cell.compile(&spec.wasm)?;
        let mut instance = cell.instantiate(&module, limits.memory_pages, spec.outputs, &runtime.budget(&limits))?;
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let config = super::plugin_node::resolve_config(spec, None)?;
        if let Err(err) = instance.init(&config, &runtime.budget(&limits), &cancel) {
            return Err(store_err(format!("selftest: {err} (with the default configuration)")));
        }
        for (i, test) in manifest.selftest.iter().enumerate() {
            let fail = |why: String| store_err(format!("selftest[{i}]: {why}"));
            let input = Variant::deserialize(&test.input).map_err(|err| fail(err.to_string()))?;
            let bytes = encode_variant(&input).map_err(|err| fail(err.to_string()))?;
            let output = match instance.call(&bytes, &runtime.budget(&limits), &cancel) {
                Ok(output) => output,
                Err(CallError::Guest { text, .. }) => return Err(fail(format!("guest failed: {text}"))),
                Err(err) => return Err(fail(err.to_string())),
            };
            let mut counts = vec![0u32; spec.outputs as usize];
            for (port, payload) in &output.outputs {
                match decode_variant(payload, runtime.max_input_bytes() as u32) {
                    Ok(Variant::Object(_)) => counts[*port as usize] += 1,
                    Ok(_) => return Err(fail(format!("output on port {port} is not an object"))),
                    Err(err) => return Err(fail(format!("output on port {port}: {err}"))),
                }
            }
            if counts != test.expect_outputs {
                return Err(fail(format!("emitted {counts:?}, expected {:?}", test.expect_outputs)));
            }
        }
        Ok(())
    }

    /// Load and verify one generation from `store/`.
    fn load_spec(&self, sha: &str) -> crate::Result<PluginSpec> {
        let bytes = read(&self.path("store", sha, "wasm"))?;
        let actual = hex(&Sha256::digest(&bytes));
        if actual != sha {
            return Err(store_err(format!("store/{sha}.wasm has sha256 {actual}; plugin disabled")));
        }
        let manifest = Manifest::parse(&manifest_text(&bytes, usize::MAX)?)?;
        PluginSpec::from_package(bytes, &manifest)
    }

    /// The plugin set `active.toml` describes. A generation that is missing or fails its digest
    /// is left out and reported; flows that use it then fail to deploy, naming the plugin.
    pub fn active_plugins(&self) -> crate::Result<(Arc<ActivePlugins>, Vec<String>)> {
        let active = self.read_active()?;
        let mut specs = Vec::new();
        let mut problems = Vec::new();
        for (id, entry) in &active.plugins {
            match self.load_spec(&entry.current) {
                Ok(spec) if &spec.id == id => specs.push(spec),
                Ok(spec) => problems.push(format!("{id}: store/{}.wasm is plugin {}", entry.current, spec.id)),
                Err(err) => problems.push(format!("{id}: {err}")),
            }
        }
        Ok((ActivePlugins::from_specs(specs), problems))
    }

    fn candidate(&self, active: &ActiveFile, id: &str, spec: PluginSpec) -> crate::Result<Arc<ActivePlugins>> {
        let mut specs = vec![spec];
        for (other, entry) in &active.plugins {
            if other != id {
                specs.push(self.load_spec(&entry.current)?);
            }
        }
        if specs.len() > self.settings.max_plugins as usize {
            return Err(store_err(format!(
                "activating {id} would make {} active plugins, above [runtime.wasm] max_plugins = {}",
                specs.len(),
                self.settings.max_plugins
            )));
        }
        Ok(ActivePlugins::from_specs(specs))
    }

    /// Make a staged, self-tested generation current. `prepare` must accept the resulting set
    /// (the deployed graph builds with it) before anything on disk changes.
    pub fn activate(&self, id: &str, sha: &str, prepare: PrepareFn<'_>) -> crate::Result<ActiveEntry> {
        let (entry, pending) = self.activate_pending(id, sha, prepare)?;
        self.finish(pending)?;
        Ok(entry)
    }

    /// [`Self::activate`] without deleting the superseded generation yet.
    pub fn activate_pending(
        &self,
        id: &str,
        sha: &str,
        prepare: PrepareFn<'_>,
    ) -> crate::Result<(ActiveEntry, PendingChange)> {
        split_id(id)?;
        valid_sha(sha)?;
        let in_store = self.report("store", sha)?;
        let report = match in_store.clone() {
            Some(report) => report,
            None => self.report("quarantine", sha)?.ok_or_else(|| store_err(format!("no staged package {sha}")))?,
        };
        if report.id != id {
            return Err(store_err(format!("package {sha} is plugin {}, not {id}", report.id)));
        }
        if report.status != PackageStatus::Ready {
            return Err(store_err(format!(
                "package {sha} was rejected: {}",
                report.reason.unwrap_or_else(|| "self-test failed".to_owned())
            )));
        }
        let mut active = self.read_active()?;
        let before = active.clone();
        if active.plugins.get(id).is_some_and(|e| e.current == sha) {
            return Ok((active.plugins[id].clone(), PendingChange { before }));
        }
        let area = if in_store.is_some() { "store" } else { "quarantine" };
        let bytes = read(&self.path(area, sha, "wasm"))?;
        if hex(&Sha256::digest(&bytes)) != sha {
            return Err(store_err(format!("{area}/{sha}.wasm does not match its digest")));
        }
        let manifest = Manifest::parse(&manifest_text(&bytes, usize::MAX)?)?;
        let spec = PluginSpec::from_package(bytes, &manifest)?;
        prepare(self.candidate(&active, id, spec)?)?;
        self.step("prepared")?;
        if in_store.is_none() {
            rename(&self.path("quarantine", sha, "wasm"), &self.path("store", sha, "wasm"))?;
            self.step("promoted-wasm")?;
            rename(&self.path("quarantine", sha, "json"), &self.path("store", sha, "json"))?;
        }
        let previous = active.plugins.get(id).map(|e| e.current.clone());
        let entry = ActiveEntry { current: sha.to_owned(), previous, activated_at: now() };
        active.plugins.insert(id.to_owned(), entry.clone());
        self.write_active(&active)?;
        self.step("pointer-written")?;
        Ok((entry, PendingChange { before }))
    }

    /// Make a pointer change final: generations it superseded are deleted.
    pub fn finish(&self, pending: PendingChange) -> crate::Result<()> {
        let now = Self::referenced(&self.read_active()?);
        for old in Self::referenced(&pending.before).difference(&now) {
            for ext in ["wasm", "json"] {
                let path = self.path("store", old, ext);
                if path.exists() {
                    fs::remove_file(&path).map_err(|e| io_err(&path, e))?;
                }
            }
        }
        Ok(())
    }

    /// Undo a pointer change: the previous `active.toml` is written back and a generation that
    /// only the undone pointer referenced returns to quarantine (still `ready`).
    pub fn revert(&self, pending: PendingChange) -> crate::Result<()> {
        let undone = Self::referenced(&self.read_active()?);
        self.write_active(&pending.before)?;
        let kept = Self::referenced(&pending.before);
        for sha in undone.difference(&kept) {
            for ext in ["wasm", "json"] {
                let from = self.path("store", sha, ext);
                if from.exists() {
                    rename(&from, &self.path("quarantine", sha, ext))?;
                }
            }
        }
        Ok(())
    }

    /// Swap `current` and `previous`. `expected_previous` guards against a stale view.
    pub fn rollback(&self, id: &str, expected_previous: &str, prepare: PrepareFn<'_>) -> crate::Result<ActiveEntry> {
        let (entry, pending) = self.rollback_pending(id, expected_previous, prepare)?;
        self.finish(pending)?;
        Ok(entry)
    }

    /// [`Self::rollback`] as a revertible pointer change.
    pub fn rollback_pending(
        &self,
        id: &str,
        expected_previous: &str,
        prepare: PrepareFn<'_>,
    ) -> crate::Result<(ActiveEntry, PendingChange)> {
        split_id(id)?;
        valid_sha(expected_previous)?;
        let mut active = self.read_active()?;
        let before = active.clone();
        let entry = active.plugins.get(id).cloned().ok_or_else(|| store_err(format!("plugin {id} is not active")))?;
        let previous =
            entry.previous.clone().ok_or_else(|| store_err(format!("plugin {id} has no previous generation")))?;
        if previous != expected_previous {
            return Err(store_err(format!("plugin {id}'s previous generation is {previous}, not {expected_previous}")));
        }
        let spec = self.load_spec(&previous)?;
        prepare(self.candidate(&active, id, spec)?)?;
        let swapped = ActiveEntry { current: previous, previous: Some(entry.current), activated_at: now() };
        active.plugins.insert(id.to_owned(), swapped.clone());
        self.write_active(&active)?;
        Ok((swapped, PendingChange { before }))
    }

    /// Deactivate `id`. Its generations move back to quarantine, preserved but never run.
    /// The caller checks first that no deployed node uses the plugin.
    pub fn remove(&self, id: &str) -> crate::Result<()> {
        split_id(id)?;
        let mut active = self.read_active()?;
        let entry = active.plugins.remove(id).ok_or_else(|| store_err(format!("plugin {id} is not active")))?;
        self.write_active(&active)?;
        for sha in std::iter::once(entry.current).chain(entry.previous) {
            for ext in ["wasm", "json"] {
                let from = self.path("store", &sha, ext);
                if from.exists() {
                    rename(&from, &self.path("quarantine", &sha, ext))?;
                }
            }
        }
        Ok(())
    }

    /// Delete a quarantined (non-active) package.
    pub fn discard(&self, sha: &str) -> crate::Result<()> {
        valid_sha(sha)?;
        let mut found = false;
        for ext in ["wasm", "json"] {
            let path = self.path("quarantine", sha, ext);
            if path.exists() {
                fs::remove_file(&path).map_err(|e| io_err(&path, e))?;
                found = true;
            }
        }
        if found { Ok(()) } else { Err(store_err(format!("no quarantined package {sha}"))) }
    }

    pub fn list(&self) -> crate::Result<Listing> {
        let active = self.read_active()?;
        let mut packages = self.quarantined()?;
        for sha in Self::referenced(&active) {
            if let Some(report) = self.report("store", &sha)? {
                packages.push(report);
            }
        }
        Ok(Listing { active: active.plugins, packages })
    }

    /// Re-hash every generation `active.toml` references; report problems instead of fixing.
    pub fn verify(&self) -> crate::Result<Vec<String>> {
        let active = self.read_active()?;
        let mut problems = Vec::new();
        for (id, entry) in &active.plugins {
            for sha in std::iter::once(&entry.current).chain(entry.previous.iter()) {
                match self.load_spec(sha) {
                    Ok(spec) if &spec.id == id => {}
                    Ok(spec) => problems.push(format!("{id}: store/{sha}.wasm is plugin {}", spec.id)),
                    Err(err) => problems.push(format!("{id}: {err}")),
                }
            }
        }
        Ok(problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::wasm::manifest::SAMPLE;
    use crate::runtime::wasm::section::append_manifest;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn temp() -> TempDir {
        let dir = std::env::temp_dir().join(format!("n2link-wasm-store-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn package(version: &str) -> Vec<u8> {
        let module = wat::parse_str(include_str!("fixtures/identity.wat")).unwrap();
        append_manifest(&module, &SAMPLE.replace("version = \"1.2.0\"", &format!("version = \"{version}\""))).unwrap()
    }

    fn open(dir: &TempDir) -> PluginStore {
        PluginStore::open_at(dir.0.join("plugins"), WasmSettings::default()).unwrap()
    }

    fn ok(_: Arc<ActivePlugins>) -> crate::Result<()> {
        Ok(())
    }

    #[test]
    fn stage_activate_upgrade_and_rollback() {
        let dir = temp();
        let store = open(&dir);
        let a = store.stage(&package("1.0.0")).unwrap();
        assert_eq!(a.status, PackageStatus::Ready, "{a:?}");
        assert_eq!(store.stage(&package("1.0.0")).unwrap(), a, "re-stage is a no-op");
        store.activate("acme/upper", &a.sha256, &ok).unwrap();
        let b = store.stage(&package("1.1.0")).unwrap();
        let entry = store.activate("acme/upper", &b.sha256, &ok).unwrap();
        assert_eq!(entry.previous.as_deref(), Some(a.sha256.as_str()));
        let (set, problems) = store.active_plugins().unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(set.get("wasm-acme-upper").unwrap().version, semver::Version::new(1, 1, 0));
        let back = store.rollback("acme/upper", &a.sha256, &ok).unwrap();
        assert_eq!(back.current, a.sha256);
        assert_eq!(back.previous.as_deref(), Some(b.sha256.as_str()));
        assert!(store.verify().unwrap().is_empty());
    }

    #[test]
    fn a_reverted_activation_restores_the_pointer_and_requarantines_the_candidate() {
        let dir = temp();
        let store = open(&dir);
        let a = store.stage(&package("1.0.0")).unwrap().sha256;
        let b = store.stage(&package("1.1.0")).unwrap().sha256;
        let c = store.stage(&package("1.2.0")).unwrap().sha256;
        store.activate("acme/upper", &a, &ok).unwrap();
        store.activate("acme/upper", &b, &ok).unwrap();
        let (entry, pending) = store.activate_pending("acme/upper", &c, &ok).unwrap();
        assert_eq!(entry.previous.as_deref(), Some(b.as_str()));
        // The generation C supersedes is kept until the change is final.
        assert!(store.root().join("store").join(format!("{a}.wasm")).exists());
        store.revert(pending).unwrap();
        let listing = store.list().unwrap();
        let active = &listing.active["acme/upper"];
        assert_eq!((active.current.as_str(), active.previous.as_deref()), (b.as_str(), Some(a.as_str())));
        assert!(store.root().join("quarantine").join(format!("{c}.wasm")).exists());
        assert!(!store.root().join("store").join(format!("{c}.wasm")).exists());
        let (set, problems) = store.active_plugins().unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(set.len(), 1);
        // C stays activatable.
        store.activate("acme/upper", &c, &ok).unwrap();
        assert!(!store.root().join("store").join(format!("{a}.wasm")).exists());
    }

    #[test]
    fn failed_activation_keeps_b_current_and_a_previous() {
        let dir = temp();
        let store = open(&dir);
        let a = store.stage(&package("1.0.0")).unwrap().sha256;
        let b = store.stage(&package("1.1.0")).unwrap().sha256;
        let c = store.stage(&package("1.2.0")).unwrap().sha256;
        store.activate("acme/upper", &a, &ok).unwrap();
        store.activate("acme/upper", &b, &ok).unwrap();
        let refuse = |_: Arc<ActivePlugins>| -> crate::Result<()> { Err(N2linkError::invalid_operation("graph")) };
        assert!(store.activate("acme/upper", &c, &refuse).is_err());
        let entry = &store.list().unwrap().active["acme/upper"];
        assert_eq!((entry.current.as_str(), entry.previous.as_deref()), (b.as_str(), Some(a.as_str())));
        assert!(store.path("quarantine", &c, "wasm").exists(), "C stays quarantined");
    }

    #[test]
    fn every_crash_point_leaves_a_complete_old_or_new_pointer() {
        for step in ["prepared", "promoted-wasm", "pointer-prev-written", "pointer-tmp-written", "pointer-written"] {
            let dir = temp();
            let a;
            let b;
            {
                let store = open(&dir);
                a = store.stage(&package("1.0.0")).unwrap().sha256;
                b = store.stage(&package("1.1.0")).unwrap().sha256;
                store.activate("acme/upper", &a, &ok).unwrap();
                store.fail_at(step);
                let _ = store.activate("acme/upper", &b, &ok);
            } // "crash": drop the store and its lock
            let store = open(&dir);
            let (set, problems) = store.active_plugins().unwrap();
            assert!(problems.is_empty(), "{step}: {problems:?}");
            let current = hex(&set.get("wasm-acme-upper").unwrap().sha256);
            assert!(current == a || current == b, "{step}");
            assert!(store.verify().unwrap().is_empty(), "{step}");
            assert!(!store.root.join(ACTIVE_TMP).exists(), "{step}");
            let staged: Vec<_> = fs::read_dir(store.root.join("staging")).unwrap().collect();
            assert!(staged.is_empty(), "{step}");
            // Whatever is not current is still available: re-activating B works.
            store.activate("acme/upper", &b, &ok).unwrap();
        }
    }

    #[test]
    fn crash_during_stage_leaves_nothing_runnable() {
        for step in ["staged", "quarantined"] {
            let dir = temp();
            {
                let store = open(&dir);
                store.fail_at(step);
                assert!(store.stage(&package("1.0.0")).is_err());
            }
            let store = open(&dir);
            assert!(store.list().unwrap().packages.is_empty(), "{step}");
            assert!(store.active_plugins().unwrap().0.get("wasm-acme-upper").is_none());
        }
    }

    #[test]
    fn invalid_packages_are_rejected_and_leave_nothing() {
        let dir = temp();
        let store = open(&dir);
        let module = wat::parse_str(include_str!("fixtures/identity.wat")).unwrap();
        assert!(store.stage(&module).unwrap_err().to_string().contains("no n2link.manifest"));
        let wasi = wat::parse_str(r#"(module (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32))) (memory (export "memory") 1) (func (export "el_abi_version") (result i32) i32.const 1) (func (export "el_alloc") (param i32) (result i32) i32.const 0) (func (export "el_on_input") (param i32 i32) (result i32) i32.const 0))"#).unwrap();
        let err = store.stage(&append_manifest(&wasi, SAMPLE).unwrap()).unwrap_err().to_string();
        assert!(err.contains("not granted"), "{err}");
        let greedy = append_manifest(&module, &SAMPLE.replace("memory_pages = 4", "memory_pages = 9999")).unwrap();
        assert!(store.stage(&greedy).unwrap_err().to_string().contains("max_memory_pages"));
        assert!(store.list().unwrap().packages.is_empty());
        assert_eq!(fs::read_dir(store.root.join("staging")).unwrap().count(), 0);
    }

    #[test]
    fn initial_memory_above_the_plugin_limit_is_rejected_with_a_fix() {
        let dir = temp();
        let store = open(&dir);
        let big = include_str!("fixtures/identity.wat")
            .replace("(memory (export \"memory\") 1 1)", "(memory (export \"memory\") 9 9)");
        let module = wat::parse_str(&big).unwrap();
        let err = store.stage(&append_manifest(&module, SAMPLE).unwrap()).unwrap_err().to_string();
        assert!(err.contains("starts with 9 pages") && err.contains("memory_pages = 9"), "{err}");
        let defaults = append_manifest(&module, &SAMPLE.replace("memory_pages = 4\n", "")).unwrap();
        let err = store.stage(&defaults).unwrap_err().to_string();
        assert!(err.contains("may use 8"), "{err}");
        let asked = append_manifest(&module, &SAMPLE.replace("memory_pages = 4", "memory_pages = 9")).unwrap();
        assert_eq!(store.stage(&asked).unwrap().status, PackageStatus::Ready);
    }

    #[test]
    fn failing_selftest_is_quarantined_as_rejected() {
        let dir = temp();
        let store = open(&dir);
        let module = wat::parse_str(include_str!("fixtures/identity.wat")).unwrap();
        let wrong = append_manifest(&module, &SAMPLE.replace("expect_outputs = [1]", "expect_outputs = [2]")).unwrap();
        let report = store.stage(&wrong).unwrap();
        assert_eq!(report.status, PackageStatus::Rejected);
        assert!(report.reason.as_deref().unwrap_or_default().contains("emitted [1], expected [2]"), "{report:?}");
        assert!(store.activate("acme/upper", &report.sha256, &ok).unwrap_err().to_string().contains("rejected"));
        store.discard(&report.sha256).unwrap();
        assert!(store.list().unwrap().packages.is_empty());
    }

    #[test]
    fn remove_preserves_packages_and_the_lock_is_exclusive() {
        let dir = temp();
        let store = open(&dir);
        let a = store.stage(&package("1.0.0")).unwrap().sha256;
        store.activate("acme/upper", &a, &ok).unwrap();
        let err = PluginStore::open_at(dir.0.join("plugins"), WasmSettings::default()).unwrap_err();
        assert!(err.to_string().contains("in use"), "{err}");
        store.remove("acme/upper").unwrap();
        assert!(store.active_plugins().unwrap().0.get("wasm-acme-upper").is_none());
        assert!(store.path("quarantine", &a, "wasm").exists());
        store.activate("acme/upper", &a, &ok).unwrap();
    }

    #[test]
    fn tampering_disables_only_that_plugin() {
        let dir = temp();
        let store = open(&dir);
        let a = store.stage(&package("1.0.0")).unwrap().sha256;
        store.activate("acme/upper", &a, &ok).unwrap();
        let path = store.path("store", &a, "wasm");
        let mut bytes = fs::read(&path).unwrap();
        bytes.push(0);
        fs::write(&path, bytes).unwrap();
        let (set, problems) = store.active_plugins().unwrap();
        assert!(set.get("wasm-acme-upper").is_none());
        assert!(problems[0].contains("plugin disabled"), "{problems:?}");
        assert_eq!(store.verify().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_store_is_refused() {
        let dir = temp();
        let real = dir.0.join("real");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, dir.0.join("plugins")).unwrap();
        let err = PluginStore::open_at(dir.0.join("plugins"), WasmSettings::default()).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    #[test]
    fn traversal_ids_and_digests_are_rejected() {
        let dir = temp();
        let store = open(&dir);
        assert!(store.activate("../x/y", &"a".repeat(64), &ok).is_err());
        assert!(store.activate("acme/upper", "../../etc/passwd", &ok).is_err());
        assert!(store.discard("../store/x").is_err());
    }
}
