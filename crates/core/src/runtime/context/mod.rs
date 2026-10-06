use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Weak},
};

use async_trait::async_trait;
use dashmap::DashMap;
use nom::Parser;
use propex::PropexSegment;
use serde;

use crate::*;
use runtime::model::*;

mod localfs;
mod memory;

pub const GLOBAL_CONTEXT_NAME: &str = "global";
pub const DEFAULT_STORE_NAME: &str = "default";
pub const DEFAULT_STORE_NAME_ALIAS: &str = "_";

type StoreFactoryFn = fn(name: String, options: Option<&ContextStoreOptions>) -> crate::Result<Box<dyn ContextStore>>;

#[derive(Debug, Clone, Copy)]
pub struct ProviderMetadata {
    pub type_: &'static str,
    pub factory: StoreFactoryFn,
}

inventory::collect!(ProviderMetadata);

#[derive(Debug, Clone, serde:: Deserialize)]
pub struct ContextStorageSettings {
    pub default: String,
    pub stores: HashMap<String, ContextStoreOptions>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ContextStoreOptions {
    /// The registered `ProviderMetadata::type_` of the store to construct.
    pub provider: String,

    /// Every other key of the store's configuration table, handed to the provider's factory.
    ///
    /// [`ContextManagerBuilder::with_config`] adds a `settings` entry holding the runtime's
    /// top-level settings (`{"userDir": <home dir>}`), which is what Node-RED copies into every
    /// store's config before constructing it.
    #[serde(flatten, default)]
    pub options: HashMap<String, config::Value>,
}

impl ContextStoreOptions {
    /// Name the provider and its options directly, as the Python bridge and the tests do,
    /// instead of reading them out of the configuration files.
    ///
    /// `options` is a plain JSON object; `null` means "no options at all".
    pub fn from_json_options(provider: &str, options: serde_json::Value) -> crate::Result<Self> {
        let options = match options {
            serde_json::Value::Null => config::Map::default(),
            serde_json::Value::Object(object) => object
                .into_iter()
                .map(|(key, value)| {
                    let value = serde::Deserialize::deserialize(value)
                        .map_err(|e| anyhow::anyhow!("Invalid option '{key}': {e}"))?;
                    Ok((key, value))
                })
                .collect::<crate::Result<config::Map<String, config::Value>>>()?,
            other => {
                return Err(N2linkError::BadArgument("options"))
                    .with_context(|| format!("The store options must be an object, but got: {other}"));
            }
        };
        Ok(Self { provider: provider.to_owned(), options: options.into_iter().collect() })
    }

    /// Deserialise the provider-specific options that follow `provider` into a typed struct.
    ///
    /// The options arrive as one `config::Value` per key because they come from a flat TOML
    /// table, so they are re-assembled into a table first to let the provider's own `serde`
    /// type pick out the keys it knows.
    pub fn deserialize_options<T: serde::de::DeserializeOwned>(&self) -> crate::Result<T> {
        let table: config::Map<String, config::Value> = self.options.clone().into_iter().collect();
        let value = config::Value::new(None, config::ValueKind::Table(table));
        value.try_deserialize::<T>().map_err(N2linkError::from)
    }
}

/// Build one context store from a provider name and its options.
///
/// This is the entry point the compiler's `inventory` registry is resolved through, exposed so
/// that the Python bridge can construct a store without a whole engine around it.
pub fn create_context_store(name: &str, options: &ContextStoreOptions) -> crate::Result<Box<dyn ContextStore>> {
    let metadata = inventory::iter::<ProviderMetadata>
        .into_iter()
        .find(|x| x.type_ == options.provider)
        .ok_or(N2linkError::Configuration)
        .with_context(|| format!("Unknown context store provider: '{}'", options.provider))?;
    (metadata.factory)(name.to_owned(), Some(options))
}

#[derive(Debug, Clone, Copy)]
pub struct ContextKey<'a> {
    pub store: Option<&'a str>,
    pub key: &'a str,
}

/// The API trait for a context storage plug-in
#[async_trait]
pub trait ContextStore: Send + Sync {
    async fn name(&self) -> &str;

    async fn open(&self) -> Result<()>;
    async fn close(&self) -> Result<()>;

    async fn get_one(&self, scope: &str, path: &[PropexSegment]) -> Result<Variant>;
    async fn get_many(&self, scope: &str, keys: &[&str]) -> Result<Vec<Variant>>;
    async fn get_keys(&self, scope: &str) -> Result<Vec<String>>;

    async fn set_one(&self, scope: &str, path: &[PropexSegment], value: Variant) -> Result<()>;
    async fn set_many(&self, scope: &str, pairs: Vec<(String, Variant)>) -> Result<()>;

    async fn remove_one(&self, scope: &str, path: &[PropexSegment]) -> Result<Variant>;

    async fn delete(&self, scope: &str) -> Result<()>;
    async fn clean(&self, active_nodes: &[ElementId]) -> Result<()>;
}

/// A context instance, allowed to bind to a flows element
#[derive(Debug, Clone)]
pub struct Context {
    inner: Arc<InnerContext>,
}

#[derive(Debug, Clone)]
pub struct WeakContext {
    inner: Weak<InnerContext>,
}

impl WeakContext {
    pub fn upgrade(&self) -> Option<Context> {
        Weak::upgrade(&self.inner).map(|x| Context { inner: x })
    }
}

#[derive(Debug)]
struct InnerContext {
    pub _parent: Option<WeakContext>,
    pub scope: String,
    manager: Weak<ContextManager>,
}

pub type ContextStoreHandle = Arc<dyn ContextStore>;

pub struct ContextManager {
    default_store: ContextStoreHandle,
    stores: HashMap<String, ContextStoreHandle>,
    contexts: DashMap<String, Context>,
    /// Online forces, keyed by scope, store name, and top-level key.
    ///
    /// A force lives with the process. It is not written into the store, so a restart releases
    /// it and the last value the store actually holds is what reads return again.
    forces: DashMap<ForceId, Variant>,
    written_at: DashMap<ForceId, i64>,
}

/// One context value as a reader sees it, including an online force.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextValue {
    pub value: Variant,
    pub forced: bool,
    /// Unix milliseconds of the last write of this key in this process. A force does not move it.
    pub updated_at: Option<i64>,
}

/// One context key and when this process last wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextAge {
    pub scope: String,
    pub store: String,
    pub key: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ForceId {
    scope: String,
    store: String,
    key: String,
}

pub struct ContextManagerBuilder {
    stores: HashMap<String, ContextStoreHandle>,
    default_store: String,
    settings: Option<ContextStorageSettings>,
}

impl Context {
    pub fn downgrade(&self) -> WeakContext {
        WeakContext { inner: Arc::downgrade(&self.inner) }
    }

    pub fn manager(&self) -> Option<Arc<ContextManager>> {
        self.inner.manager.upgrade()
    }

    pub async fn get_one(&self, storage: Option<&str>, key: &str, eval_env: &[PropexEnv<'_>]) -> Option<Variant> {
        let manager = self.inner.manager.upgrade()?;
        let store_name = resolved_store_name(&manager, storage)?;
        let store = manager.configured_store(&store_name)?.clone();
        // TODO FIXME change it to fixed length stack-allocated string
        let mut path = propex::parse(key).ok()?;
        expand_propex_segments(&mut path, eval_env).ok()?;
        if let Some(root) = root_property(&path)
            && manager.is_forced(&self.inner.scope, &store_name, root)
        {
            let forced = manager.forced_value(&self.inner.scope, &store_name, root)?;
            return if path.len() == 1 { Some(forced) } else { forced.get_segs(&path[1..]).cloned() };
        }
        store.get_one(&self.inner.scope, &path).await.ok()
    }

    pub async fn keys(&self, store: Option<&str>) -> Option<Vec<String>> {
        let manager = self.inner.manager.upgrade()?;
        let store_name = resolved_store_name(&manager, store)?;
        let store = manager.configured_store(&store_name)?;
        let stored = store.get_keys(&self.inner.scope).await;
        let mut names = match &stored {
            Ok(keys) => keys.clone(),
            Err(err) if err.is_out_of_range() => Vec::new(),
            Err(_) => return None,
        };
        for (key, _) in manager.forces_in(&self.inner.scope, &store_name) {
            if !names.contains(&key) {
                names.push(key);
            }
        }
        if names.is_empty() && stored.is_err() {
            // A scope the memory store has never seen used to be `None`, which the function
            // node reports as undefined. An empty list from a store that answered `Ok` stays empty.
            return None;
        }
        Some(names)
    }

    pub async fn set_one(
        &self,
        storage: Option<&str>,
        key: &str,
        value: Option<Variant>,
        eval_env: &[PropexEnv<'_>],
    ) -> Result<()> {
        let manager = self.inner.manager.upgrade().expect("manager");
        let store_name = resolved_store_name_required(&manager, storage)?;
        let store = manager
            .configured_store(&store_name)
            .cloned()
            .ok_or(N2linkError::BadArgument("storage"))
            .with_context(|| format!("Cannot found the storage: '{store_name}'"))?;
        let mut path = propex::parse(key)?;
        expand_propex_segments(&mut path, eval_env)?;
        if let Some(root) = root_property(&path)
            && manager.is_forced(&self.inner.scope, &store_name, root)
        {
            // A held key stays at the forced value. Removing it is refused: delete is not clear-force.
            return if value.is_some() {
                Ok(())
            } else {
                Err(N2linkError::InvalidOperation(format!("context key '{root}' is forced")))
            };
        }
        let removing = value.is_none();
        if let Some(value) = value {
            store.set_one(&self.inner.scope, &path, value).await?;
        } else {
            let _ = store.remove_one(&self.inner.scope, &path).await?;
        }
        if let Some(root) = root_property(&path) {
            if removing && path.len() == 1 {
                manager.forget_write(&self.inner.scope, &store_name, root);
            } else {
                manager.note_write(&self.inner.scope, &store_name, root);
            }
        }
        Ok(())
    }

    /// Hold `key` at `value` until [`Self::clear_force`]. Writes of that key are ignored meanwhile.
    ///
    /// `key` is one property, the same name the context sidebar lists. A nested path is rejected.
    pub fn force_one(&self, storage: Option<&str>, key: &str, value: Variant) -> Result<()> {
        let manager = self.inner.manager.upgrade().expect("manager");
        let (store_name, root) = force_target(&manager, storage, key)?;
        manager.hold_force(&self.inner.scope, &store_name, &root, value);
        Ok(())
    }

    /// Release a force. The next read is the value the store held underneath, and writes land again.
    pub fn clear_force(&self, storage: Option<&str>, key: &str) -> Result<()> {
        let manager = self.inner.manager.upgrade().expect("manager");
        let (store_name, root) = force_target(&manager, storage, key)?;
        manager.release_force(&self.inner.scope, &store_name, &root);
        Ok(())
    }

    /// Top-level entries in one store.
    ///
    /// A scope that has never been written is an empty list. The memory store reports that as
    /// `OutOfRange`; the editor sidebar treats "no keys" as an empty table, not as a failure.
    /// `store_name` is the configured name (`memory`, not the `default` alias).
    pub async fn read_store(&self, store_name: &str, store: &dyn ContextStore) -> Result<Vec<(String, ContextValue)>> {
        let keys = match store.get_keys(&self.inner.scope).await {
            Ok(keys) => keys,
            Err(err) if err.is_out_of_range() => Vec::new(),
            Err(err) => return Err(err),
        };
        let manager = self.inner.manager.upgrade().expect("manager");
        let mut entries = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(value) = manager.forced_value(&self.inner.scope, store_name, &key) {
                let updated_at = manager.written_at(&self.inner.scope, store_name, &key);
                entries.push((key, ContextValue { value, forced: true, updated_at }));
                continue;
            }
            if let Some(value) = self.read_parsed(store, &key).await? {
                let updated_at = manager.written_at(&self.inner.scope, store_name, &key);
                entries.push((key, ContextValue { value, forced: false, updated_at }));
            }
        }
        for (key, value) in manager.forces_in(&self.inner.scope, store_name) {
            if entries.iter().any(|(existing, _)| existing == &key) {
                continue;
            }
            entries.push((key, ContextValue { value, forced: true, updated_at: None }));
        }
        Ok(entries)
    }

    /// One key in one store. `Ok(None)` is a key that is not there and not forced.
    ///
    /// `store_name` is the configured name (`memory`, not the `default` alias).
    pub async fn read_key(
        &self,
        store_name: &str,
        store: &dyn ContextStore,
        key: &str,
    ) -> Result<Option<ContextValue>> {
        let manager = self.inner.manager.upgrade().expect("manager");
        if let Some(value) = manager.forced_value(&self.inner.scope, store_name, key) {
            let updated_at = manager.written_at(&self.inner.scope, store_name, key);
            return Ok(Some(ContextValue { value, forced: true, updated_at }));
        }
        let updated_at = manager.written_at(&self.inner.scope, store_name, key);
        Ok(self.read_parsed(store, key).await?.map(|value| ContextValue { value, forced: false, updated_at }))
    }

    async fn read_parsed(&self, store: &dyn ContextStore, key: &str) -> Result<Option<Variant>> {
        let path = propex::parse(key)?;
        match store.get_one(&self.inner.scope, &path).await {
            Ok(value) => Ok(Some(value)),
            Err(err) if err.is_out_of_range() => Ok(None),
            Err(err) => Err(err),
        }
    }
}

impl Default for ContextManager {
    fn default() -> Self {
        let x = inventory::iter::<ProviderMetadata>;
        let memory_metadata = x.into_iter().find(|x| x.type_ == "memory").unwrap();
        let memory_store =
            (memory_metadata.factory)("memory".into(), None).expect("Create memory storage cannot go wrong.");
        let mut stores: HashMap<std::string::String, ContextStoreHandle> = HashMap::with_capacity(1);
        stores.insert("memory".to_owned(), Arc::from(memory_store));
        Self {
            default_store: stores["memory"].clone(),
            contexts: DashMap::new(),
            stores,
            forces: DashMap::new(),
            written_at: DashMap::new(),
        }
    }
}

impl Default for ContextManagerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextManagerBuilder {
    pub fn new() -> Self {
        let stores = HashMap::with_capacity(inventory::iter::<ProviderMetadata>.into_iter().count());
        Self { stores, default_store: "memory".into(), settings: None }
    }

    pub fn load_default(&mut self) -> &mut Self {
        let memory_metadata = inventory::iter::<ProviderMetadata>.into_iter().find(|x| x.type_ == "memory").unwrap();
        let memory_store =
            (memory_metadata.factory)("memory".into(), None).expect("Create memory storage cannot go wrong.");
        self.stores.clear();
        self.stores.insert("memory".to_owned(), Arc::from(memory_store));
        self
    }

    pub fn with_config(&mut self, config: &config::Config) -> crate::Result<&mut Self> {
        let mut settings: ContextStorageSettings = config.get("runtime.context")?;
        if !settings.stores.contains_key(&settings.default) {
            use crate::ErrorContext as _;
            return Err(N2linkError::Configuration).with_context(|| {
                format!(
                    "Cannot found the default context storage '{}', check your configuration file.",
                    settings.default
                )
            });
        }

        // Node-RED's context loader copies the top-level `userDir` setting into every store's
        // config before constructing it; a store that has no directory of its own (`localfilesystem`
        // without `dir`) resolves its storage under it.
        match config.get_string("home_dir") {
            Ok(home_dir) => {
                let user_dir = config::Value::new(None, config::ValueKind::String(home_dir));
                for store_options in settings.stores.values_mut() {
                    store_options.options.entry("settings".to_owned()).or_insert_with(|| {
                        config::Value::new(
                            None,
                            config::ValueKind::Table(config::Map::from([("userDir".to_owned(), user_dir.clone())])),
                        )
                    });
                }
            }
            Err(config::ConfigError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }

        self.stores.clear();
        for (store_name, store_options) in settings.stores.iter() {
            log::debug!(
                "[CONTEXT_MANAGER_BUILDER] Initializing context store: name='{}', provider='{}' ...",
                store_name,
                store_options.provider
            );
            let store = create_context_store(store_name, store_options).with_context(|| {
                format!("Cannot initialize the context store '{store_name}' of {}", store_options.provider)
            })?;
            self.stores.insert(store_name.clone(), Arc::from(store));
        }

        self.default_store.clone_from(&settings.default);
        self.settings = Some(settings);
        Ok(self)
    }

    pub fn default_store(&mut self, default: String) -> &mut Self {
        self.default_store = default;
        self
    }

    pub fn build(&self) -> crate::Result<Arc<ContextManager>> {
        let default_store = self
            .stores
            .get(&self.default_store)
            .ok_or(N2linkError::Configuration)
            .with_context(|| format!("Cannot found the default context store '{}'", self.default_store))?
            .clone();
        let cm = ContextManager {
            default_store,
            stores: self.stores.clone(),
            contexts: DashMap::new(),
            forces: DashMap::new(),
            written_at: DashMap::new(),
        };
        Ok(Arc::new(cm))
    }
}

impl ContextManager {
    pub fn new_context(self: &Arc<Self>, parent: &Context, scope: String) -> Context {
        let inner =
            InnerContext { _parent: Some(parent.downgrade()), manager: Arc::downgrade(self), scope: scope.clone() };
        let c = Context { inner: Arc::new(inner) };
        self.contexts.insert(scope, c.clone());
        c
    }

    pub fn new_global_context(self: &Arc<Self>) -> Context {
        let inner =
            InnerContext { _parent: None, manager: Arc::downgrade(self), scope: GLOBAL_CONTEXT_NAME.to_string() };
        let c = Context { inner: Arc::new(inner) };
        self.contexts.insert(GLOBAL_CONTEXT_NAME.to_string(), c.clone());
        c
    }

    pub fn get_default_store(&self) -> &ContextStoreHandle {
        &self.default_store
    }

    pub fn get_context_store<'a>(&'a self, store_name: &str) -> Option<&'a ContextStoreHandle> {
        match store_name {
            DEFAULT_STORE_NAME | DEFAULT_STORE_NAME_ALIAS | "" => Some(&self.default_store),
            _ => self.stores.get(store_name),
        }
    }

    /// Configured store names, in a stable order.
    pub fn store_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.stores.keys().cloned().collect();
        names.sort();
        names
    }

    /// Name of the store [`Self::get_default_store`] points at.
    pub fn default_store_name(&self) -> String {
        self.stores
            .iter()
            .find(|(_, store)| Arc::ptr_eq(*store, &self.default_store))
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| DEFAULT_STORE_NAME.to_owned())
    }

    /// The configured name a caller asked for.
    ///
    /// `default`, `_`, and an empty name select the default store, which is what
    /// [`Self::get_context_store`] already does. A name that is not configured is `None`.
    pub fn canonical_store_name(&self, requested: &str) -> Option<String> {
        match requested {
            DEFAULT_STORE_NAME | DEFAULT_STORE_NAME_ALIAS | "" => Some(self.default_store_name()),
            name => self.stores.contains_key(name).then(|| name.to_owned()),
        }
    }

    /// The store registered under `name`, with no alias applied.
    ///
    /// Listing every store has to use this. [`Self::get_context_store`] would turn a store that
    /// is itself named `default` into the default store.
    pub fn configured_store(&self, name: &str) -> Option<&ContextStoreHandle> {
        self.stores.get(name)
    }

    /// Open every configured store, the way Node-RED's context loader does on startup.
    ///
    /// A store that persists its scopes (`localfilesystem`) loads them here, so this has to
    /// happen before any flow runs and after the configured stores are known.
    pub async fn open_all(&self) -> crate::Result<()> {
        for (name, store) in self.stores.iter() {
            store.open().await.with_context(|| format!("Cannot open the context store '{name}'"))?;
        }
        Ok(())
    }

    /// Close every configured store, flushing whatever the persistent ones still hold.
    pub async fn close_all(&self) -> crate::Result<()> {
        for (name, store) in self.stores.iter() {
            store.close().await.with_context(|| format!("Cannot close the context store '{name}'"))?;
        }
        Ok(())
    }

    /// Drop the scopes that no longer belong to a deployed node or flow.
    ///
    /// Forces follow the memory store: `global` stays, and any other scope stays only when the
    /// id before `:` is still deployed. A restart drops every force, because they are not written
    /// into the store. A redeploy keeps this manager, so a removed scope must not keep one.
    pub async fn clean_all(&self, active_nodes: &[ElementId]) -> crate::Result<()> {
        for (name, store) in self.stores.iter() {
            store.clean(active_nodes).await.with_context(|| format!("Cannot clean the context store '{name}'"))?;
        }
        let active: HashSet<String> = active_nodes.iter().map(ElementId::to_string).collect();
        self.forces.retain(|id, _| {
            id.scope == GLOBAL_CONTEXT_NAME || active.contains(id.scope.split(':').next().unwrap_or(&id.scope))
        });
        Ok(())
    }

    fn force_id(scope: &str, store: &str, key: &str) -> ForceId {
        ForceId { scope: scope.to_owned(), store: store.to_owned(), key: key.to_owned() }
    }

    fn is_forced(&self, scope: &str, store: &str, key: &str) -> bool {
        self.forces.contains_key(&Self::force_id(scope, store, key))
    }

    fn forced_value(&self, scope: &str, store: &str, key: &str) -> Option<Variant> {
        self.forces.get(&Self::force_id(scope, store, key)).map(|entry| entry.value().clone())
    }

    fn forces_in(&self, scope: &str, store: &str) -> Vec<(String, Variant)> {
        self.forces
            .iter()
            .filter(|entry| entry.key().scope == scope && entry.key().store == store)
            .map(|entry| (entry.key().key.clone(), entry.value().clone()))
            .collect()
    }

    fn hold_force(&self, scope: &str, store: &str, key: &str, value: Variant) {
        self.forces.insert(Self::force_id(scope, store, key), value);
    }

    fn release_force(&self, scope: &str, store: &str, key: &str) {
        self.forces.remove(&Self::force_id(scope, store, key));
    }

    fn note_write(&self, scope: &str, store: &str, key: &str) {
        self.written_at.insert(Self::force_id(scope, store, key), unix_ms());
    }

    fn forget_write(&self, scope: &str, store: &str, key: &str) {
        self.written_at.remove(&Self::force_id(scope, store, key));
    }

    fn written_at(&self, scope: &str, store: &str, key: &str) -> Option<i64> {
        self.written_at.get(&Self::force_id(scope, store, key)).map(|entry| *entry.value())
    }

    /// Last write of every key still remembered. A restart drops the list. A force does not add one.
    pub fn ages(&self) -> Vec<ContextAge> {
        let mut ages: Vec<ContextAge> = self
            .written_at
            .iter()
            .map(|entry| ContextAge {
                scope: entry.key().scope.clone(),
                store: entry.key().store.clone(),
                key: entry.key().key.clone(),
                updated_at: *entry.value(),
            })
            .collect();
        ages.sort_by(|left, right| {
            (&left.scope, &left.store, &left.key).cmp(&(&right.scope, &right.store, &right.key))
        });
        ages
    }
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn resolved_store_name(manager: &ContextManager, storage: Option<&str>) -> Option<String> {
    match storage {
        None => Some(manager.default_store_name()),
        Some(name) => manager.canonical_store_name(name),
    }
}

fn resolved_store_name_required(manager: &ContextManager, storage: Option<&str>) -> Result<String> {
    match storage {
        None => Ok(manager.default_store_name()),
        Some(name) => manager
            .canonical_store_name(name)
            .ok_or(N2linkError::BadArgument("storage"))
            .with_context(|| format!("Cannot found the storage: '{name}'")),
    }
}

fn root_property<'a>(path: &'a [PropexSegment<'a>]) -> Option<&'a str> {
    match path.first() {
        Some(PropexSegment::Property(name)) => Some(name.as_ref()),
        _ => None,
    }
}

/// One top-level property in a configured store. A nested path cannot be forced.
fn force_target(manager: &ContextManager, storage: Option<&str>, key: &str) -> Result<(String, String)> {
    let store_name = resolved_store_name_required(manager, storage)?;
    let path = propex::parse(key)?;
    match path.as_slice() {
        [PropexSegment::Property(name)] => Ok((store_name, name.as_ref().to_owned())),
        _ => Err(N2linkError::NotSupported(format!("forcing a nested context key is not supported: '{key}'"))),
    }
}

fn parse_store_expr(input: &str) -> nom::IResult<&str, &str, nom::error::Error<&str>> {
    use crate::text::nom_parsers::*;
    use nom::{
        bytes::complete::tag,
        character::complete::{char, multispace0},
        sequence::delimited,
    };

    let (input, _) = tag("#:").parse(input)?;
    let (input, store) =
        delimited(char('('), delimited(multispace0, identifier, multispace0), char(')')).parse(input)?;
    let (input, _) = tag("::").parse(input)?;
    Ok((input, store))
}

fn context_store_parser(input: &str) -> nom::IResult<&str, ContextKey<'_>, nom::error::Error<&str>> {
    // use crate::text::nom_parsers::*;
    use nom::combinator::{opt, rest};

    let (input, store) = opt(parse_store_expr).parse(input)?;
    let (input, key) = rest(input)?;

    Ok((input, ContextKey { store, key }))
}

/// Parses a context property string, as generated by the TypedInput, to extract
/// the store name if present.
///
/// # Examples
/// For example, `#:(file)::foo.bar` results in ` ContextKey { store: Some("file"), key: "foo.bar" }`.
/// ```
/// use n2link_core::runtime::context::evaluate_key;
///
/// let res = evaluate_key("#:(file)::foo.bar").unwrap();
/// assert_eq!(Some("file"), res.store);
/// assert_eq!("foo.bar", res.key);
/// ```
pub fn evaluate_key(key: &str) -> crate::Result<ContextKey<'_>> {
    match context_store_parser(key) {
        Ok(res) => Ok(res.1),
        Err(e) => Err(N2linkError::BadArgument("key")).with_context(|| format!("Can not parse the key: '{e}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_context_store() {
        let res = evaluate_key("#:(file1)::foo.bar").unwrap();
        assert_eq!(Some("file1"), res.store);
        assert_eq!("foo.bar", res.key);

        let res = evaluate_key("#:(memory1)::payload").unwrap();
        assert_eq!(Some("memory1"), res.store);
        assert_eq!("payload", res.key);

        let res = evaluate_key("foo.bar").unwrap();
        assert_eq!(None, res.store);
        assert_eq!("foo.bar", res.key);
    }

    #[tokio::test]
    async fn test_context_manager_can_load_default_config() {
        let ctxman = ContextManagerBuilder::new().load_default().build().unwrap();
        let global = ctxman.new_global_context();
        global.set_one(None, "foo", Some(Variant::from("bar")), &[]).await.unwrap();

        let foo = global.get_one(None, "foo", &[]).await.unwrap();
        assert_eq!(foo, "bar".into());
    }

    #[test]
    fn default_memory_store_is_named_memory() {
        let ctxman = ContextManagerBuilder::new().load_default().build().unwrap();
        assert_eq!(ctxman.default_store_name(), "memory");
        assert_eq!(ctxman.store_names(), vec!["memory".to_string()]);
        assert_eq!(ctxman.canonical_store_name("default").as_deref(), Some("memory"));
        assert_eq!(ctxman.canonical_store_name("_").as_deref(), Some("memory"));
        assert_eq!(ctxman.canonical_store_name("memory").as_deref(), Some("memory"));
        assert_eq!(ctxman.canonical_store_name("disk"), None);
    }

    #[tokio::test]
    async fn read_store_is_empty_until_a_key_is_written() {
        let ctxman = ContextManagerBuilder::new().load_default().build().unwrap();
        let global = ctxman.new_global_context();
        let store = ctxman.configured_store("memory").unwrap().clone();

        let entries = global.read_store("memory", store.as_ref()).await.unwrap();
        assert!(entries.is_empty());
        assert!(global.read_key("memory", store.as_ref(), "plant").await.unwrap().is_none());

        global.set_one(None, "plant", Some(Variant::from("running")), &[]).await.unwrap();
        let entries = global.read_store("memory", store.as_ref()).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "plant");
        assert_eq!(entries[0].1.value, Variant::from("running"));
        assert!(!entries[0].1.forced);
        assert!(entries[0].1.updated_at.is_some());
        let ages = ctxman.ages();
        assert_eq!(ages.len(), 1);
        assert_eq!(ages[0].key, "plant");
    }

    #[tokio::test]
    async fn force_holds_flow_motor_until_cleared() {
        let ctxman = ContextManagerBuilder::new().load_default().build().unwrap();
        let global = ctxman.new_global_context();
        let flow = ctxman.new_context(&global, "100".into());
        flow.set_one(None, "motor", Some(Variant::from(true)), &[]).await.unwrap();

        assert!(flow.force_one(Some("disk"), "motor", Variant::from(false)).is_err());
        assert_eq!(flow.get_one(None, "motor", &[]).await.unwrap(), Variant::from(true));

        flow.force_one(None, "motor", Variant::from(false)).unwrap();
        assert_eq!(flow.get_one(None, "motor", &[]).await.unwrap(), Variant::from(false));
        // A node write of a value is accepted and ignored. Delete is not a second way to drop the hold.
        flow.set_one(None, "motor", Some(Variant::from(true)), &[]).await.unwrap();
        assert_eq!(flow.get_one(None, "motor", &[]).await.unwrap(), Variant::from(false));
        flow.set_one(Some("memory"), "motor", Some(Variant::from("run")), &[]).await.unwrap();
        assert_eq!(flow.get_one(None, "motor", &[]).await.unwrap(), Variant::from(false));
        let err = flow.set_one(None, "motor", None, &[]).await.unwrap_err();
        assert!(matches!(err, crate::N2linkError::InvalidOperation(_)));

        flow.clear_force(None, "motor").unwrap();
        assert_eq!(flow.get_one(None, "motor", &[]).await.unwrap(), Variant::from(true));
        flow.clear_force(None, "motor").unwrap();

        // `default` and `_` are the same hold as the store named `memory`.
        flow.force_one(Some("default"), "motor", Variant::from(false)).unwrap();
        assert_eq!(flow.get_one(Some("memory"), "motor", &[]).await.unwrap(), Variant::from(false));
        flow.set_one(Some("_"), "motor", Some(Variant::from(true)), &[]).await.unwrap();
        assert_eq!(flow.get_one(None, "motor", &[]).await.unwrap(), Variant::from(false));
        flow.clear_force(Some("_"), "motor").unwrap();
        assert_eq!(flow.get_one(None, "motor", &[]).await.unwrap(), Variant::from(true));

        flow.set_one(None, "motor", Some(Variant::from("run")), &[]).await.unwrap();
        assert_eq!(flow.get_one(None, "motor", &[]).await.unwrap(), Variant::from("run"));
    }

    #[tokio::test]
    async fn force_rejects_nested_paths_and_clean_drops_removed_scopes() {
        let ctxman = ContextManagerBuilder::new().load_default().build().unwrap();
        let global = ctxman.new_global_context();
        let flow_id = crate::runtime::model::ElementId::with_u64(0x100);
        let node_id = crate::runtime::model::ElementId::with_u64(0x1);
        let flow = ctxman.new_context(&global, flow_id.to_string());
        let node = ctxman.new_context(&global, format!("{node_id}:{flow_id}"));

        let stored = Variant::from(serde_json::json!({"speed": 1, "hidden": true}));
        flow.set_one(None, "pose", Some(stored.clone()), &[]).await.unwrap();
        let err = flow.force_one(None, "pose.speed", Variant::from(serde_json::json!(4))).unwrap_err();
        match err {
            crate::N2linkError::NotSupported(message) => assert!(message.contains("pose.speed"), "{message}"),
            other => panic!("expected NotSupported, got {other}"),
        }

        flow.force_one(None, "pose", Variant::from(serde_json::json!({"speed": 4}))).unwrap();
        assert_eq!(flow.get_one(None, "pose.speed", &[]).await.unwrap(), Variant::from(serde_json::json!(4)));
        // A nested read stays inside the forced value. It does not fall through to the stored field.
        assert!(flow.get_one(None, "pose.hidden", &[]).await.is_none());
        flow.set_one(None, "pose.speed", Some(Variant::from(serde_json::json!(9))), &[]).await.unwrap();
        assert_eq!(flow.get_one(None, "pose.speed", &[]).await.unwrap(), Variant::from(serde_json::json!(4)));
        flow.clear_force(None, "pose").unwrap();
        assert_eq!(flow.get_one(None, "pose", &[]).await.unwrap(), stored);

        flow.force_one(None, "lamp", Variant::from(true)).unwrap();
        let store = ctxman.configured_store("memory").unwrap();
        let seen = flow.read_key("memory", store.as_ref(), "lamp").await.unwrap().unwrap();
        assert_eq!(seen.value, Variant::from(true));
        assert!(seen.forced);
        assert!(seen.updated_at.is_none());
        flow.clear_force(None, "lamp").unwrap();
        assert!(flow.read_key("memory", store.as_ref(), "lamp").await.unwrap().is_none());

        global.force_one(None, "plant", Variant::from("held")).unwrap();
        flow.force_one(None, "motor", Variant::from(false)).unwrap();
        node.force_one(None, "count", Variant::from(1_i32)).unwrap();
        ctxman.clean_all(&[]).await.unwrap();
        assert_eq!(global.get_one(None, "plant", &[]).await.unwrap(), Variant::from("held"));
        assert!(flow.get_one(None, "motor", &[]).await.is_none());
        assert!(node.get_one(None, "count", &[]).await.is_none());

        flow.force_one(None, "motor", Variant::from(true)).unwrap();
        node.force_one(None, "count", Variant::from(2_i32)).unwrap();
        ctxman.clean_all(&[flow_id]).await.unwrap();
        assert_eq!(flow.get_one(None, "motor", &[]).await.unwrap(), Variant::from(true));
        assert!(node.get_one(None, "count", &[]).await.is_none());

        node.force_one(None, "count", Variant::from(3_i32)).unwrap();
        ctxman.clean_all(&[flow_id, node_id]).await.unwrap();
        assert_eq!(node.get_one(None, "count", &[]).await.unwrap(), Variant::from(3_i32));
        assert_eq!(global.get_one(None, "plant", &[]).await.unwrap(), Variant::from("held"));
    }
}
