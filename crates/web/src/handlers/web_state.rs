// REMOVED: clone_for_update. All mutation must use interior mutability (Mutex/RwLock) on fields.
use axum::Router;
use n2link_core::runtime::paths;

use crate::handlers::CommsManager;
use crate::handlers::audit::AuditLog;
use crate::handlers::auth::AdminAuth;
use crate::handlers::fleet::Fleet;
use crate::models::RedSystemSettings;
use n2link_core::runtime::credential_storage::CredentialStore;
use n2link_core::runtime::egress::EgressPolicyHandle;
use n2link_core::runtime::engine::Engine;
use n2link_core::runtime::engine_events::EngineEvent;
use n2link_core::runtime::registry::RegistryHandle;
use n2link_core::web::WebHandlerRegistry;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::protection::ApiProtection;

// --- WebStateCore trait implementation ---
use n2link_core::web::web_state_trait::WebStateCore;

/// Callback type for restarting the flow engine
pub type FlowEngineRestartCallback = Arc<dyn Fn(PathBuf) -> tokio::task::JoinHandle<()> + Send + Sync>;

#[derive(Clone)]
pub struct WebRuntimeServices {
    pub egress: EgressPolicyHandle,
    pub credentials: CredentialStore,
    pub protection: ApiProtection,
    pub history: n2link_core::runtime::history::HistoryHandle,
    pub copilot_strict_metadata: bool,
}

impl Default for WebRuntimeServices {
    fn default() -> Self {
        Self {
            egress: EgressPolicyHandle::default(),
            credentials: CredentialStore::default(),
            protection: ApiProtection::default(),
            history: n2link_core::runtime::history::HistoryHandle::default(),
            copilot_strict_metadata: true,
        }
    }
}

/// Application state for storing system configuration
///
/// Always use `Arc<WebState>` for sharing between handlers and layers.
pub struct WebState {
    pub red_settings: Arc<RedSystemSettings>,
    pub registry: RwLock<Option<RegistryHandle>>, // Node registry
    pub comms: CommsManager,                      // WebSocket communication manager
    pub flows_file_path: RwLock<Option<PathBuf>>, // Path to the flows.json file
    pub restart_callback: RwLock<Option<FlowEngineRestartCallback>>, // Callback to restart the engine
    pub engine: RwLock<Option<Arc<Engine>>>,      // Engine instance for direct integration
    pub cancel_token: RwLock<Option<CancellationToken>>, // Cancellation token for graceful shutdown
    pub static_dir: PathBuf,                      // Static files directory
    pub web_handlers: WebHandlerRegistry,         // Dynamic/static web handler registry
    pub auth: Arc<AdminAuth>,
    pub audit: AuditLog,
    pub fleet: Arc<Fleet>,
    pub egress: EgressPolicyHandle,
    pub credentials: CredentialStore,
    pub protection: ApiProtection,
    pub history: n2link_core::runtime::history::HistoryHandle,
    pub config_editor_enabled: bool,
    pub copilot_strict_metadata: bool,
    /// The environment-specific overlay edited by the configuration pane.
    pub config_file_path: RwLock<Option<PathBuf>>,
    /// Full-file revision whose egress policy is currently active.
    pub applied_config_rev: RwLock<String>,
    /// Process start. A redeploy replaces the engine and must not reset this clock.
    pub started_at: std::time::Instant,
    /// Address the listener actually bound. `None` until [`crate::server::WebServer::spawn`].
    pub listen: RwLock<Option<SocketAddr>>,
    /// Serializes deploy and rollback so two clients cannot both pass the same revision.
    pub deploy: tokio::sync::Mutex<()>,
    /// Serializes configuration save, apply, and rollback transactions.
    pub config_apply: tokio::sync::Mutex<()>,
    /// The WASM plugin store, present only when `[runtime.wasm] enabled = true`.
    #[cfg(feature = "nodes_wasm")]
    pub plugin_store: RwLock<Option<Arc<n2link_core::runtime::wasm::PluginStore>>>,
}

/// Implement WebStateCore trait for WebState
impl WebStateCore for WebState {
    fn engine(&self) -> &RwLock<Option<Arc<Engine>>> {
        &self.engine
    }
    fn registry(&self) -> &RwLock<Option<RegistryHandle>> {
        &self.registry
    }
    fn static_dir(&self) -> &PathBuf {
        &self.static_dir
    }
    fn web_handlers(&self) -> &WebHandlerRegistry {
        &self.web_handlers
    }
    fn flows_file_path(&self) -> &RwLock<Option<PathBuf>> {
        &self.flows_file_path
    }
    fn cancel_token(&self) -> &RwLock<Option<CancellationToken>> {
        &self.cancel_token
    }
}

impl WebState {
    /// Construct a new WebState wrapped in Arc for use everywhere.
    /// Auth is open and fleet is off, which is the default install.
    pub fn new() -> Arc<Self> {
        Self::assemble(
            Arc::new(RedSystemSettings::default()),
            Self::default_static_dir(),
            None,
            AdminAuth::open(),
            Fleet::disabled(),
        )
    }

    pub fn assemble(
        red_settings: Arc<RedSystemSettings>,
        static_dir: PathBuf,
        cancel_token: Option<CancellationToken>,
        auth: AdminAuth,
        fleet: Fleet,
    ) -> Arc<Self> {
        Self::assemble_with_egress(
            red_settings,
            static_dir,
            cancel_token,
            auth,
            fleet,
            WebRuntimeServices::default(),
            false,
        )
    }

    pub fn assemble_with_egress(
        red_settings: Arc<RedSystemSettings>,
        static_dir: PathBuf,
        cancel_token: Option<CancellationToken>,
        auth: AdminAuth,
        fleet: Fleet,
        services: WebRuntimeServices,
        config_editor_enabled: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            red_settings,
            registry: RwLock::new(None),
            comms: CommsManager::new(),
            flows_file_path: RwLock::new(None),
            restart_callback: RwLock::new(None),
            engine: RwLock::new(None),
            cancel_token: RwLock::new(cancel_token),
            static_dir,
            web_handlers: WebHandlerRegistry::new(),
            auth: Arc::new(auth),
            audit: AuditLog::new(),
            fleet: Arc::new(fleet),
            egress: services.egress,
            credentials: services.credentials,
            protection: services.protection,
            history: services.history,
            copilot_strict_metadata: services.copilot_strict_metadata,
            config_editor_enabled,
            config_file_path: RwLock::new(None),
            applied_config_rev: RwLock::new(String::new()),
            started_at: std::time::Instant::now(),
            listen: RwLock::new(None),
            deploy: tokio::sync::Mutex::new(()),
            config_apply: tokio::sync::Mutex::new(()),
            #[cfg(feature = "nodes_wasm")]
            plugin_store: RwLock::new(None),
        })
    }
}

impl WebState {
    /// Helper to determine the default static directory path
    fn default_static_dir() -> PathBuf {
        paths::ui_static_dir()
    }
}

impl WebState {
    /// Register all web_handlers routes into the given axum Router and return the new Router.
    /// Passes a reference to self (as WebStateCore) to each handler's router registration if supported.
    pub fn register_web_routes(&self, mut router: Router) -> Router {
        for desc in self.web_handlers.routes_handle().lock().unwrap().iter() {
            log::info!("Registering dynamic route: {}", desc.path);
            router = router.route(&desc.path, desc.router.clone());
        }
        router
    }
}

impl WebState {
    /// Set the node registry
    pub async fn set_registry(&self, registry: RegistryHandle) {
        let mut reg = self.registry.write().await;
        *reg = Some(registry);
    }

    /// Hand the plugin store (and its lock) to the admin API.
    #[cfg(feature = "nodes_wasm")]
    pub async fn set_plugin_store(&self, store: Arc<n2link_core::runtime::wasm::PluginStore>) {
        *self.plugin_store.write().await = Some(store);
    }

    /// Set the Engine instance
    pub async fn set_engine(&self, engine: Arc<Engine>) {
        let mut eng = self.engine.write().await;
        *eng = Some(engine);
    }

    /// Set the flows file path. The parent directory holds the previous flows, the audit log,
    /// the local library, and the fleet inventory.
    /// Record the socket the server bound, including a port chosen as 0.
    pub async fn record_listen(&self, addr: SocketAddr) {
        *self.listen.write().await = Some(addr);
    }

    pub async fn set_flows_file_path(&self, path: PathBuf) {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            self.audit.set_home(parent.to_path_buf()).await;
            if let Err(err) = self.fleet.load_home(parent.to_path_buf()).await {
                panic!("fleet configuration is not valid: {err}");
            }
            self.history.set_home(parent.to_path_buf());
        }
        let mut f = self.flows_file_path.write().await;
        *f = Some(path);
    }

    pub async fn set_config_file_path(&self, path: PathBuf) {
        let bytes = tokio::fs::read(&path).await.unwrap_or_default();
        *self.applied_config_rev.write().await = crate::handlers::runtime_config::revision(&bytes);
        *self.config_file_path.write().await = Some(path);
    }

    /// Set the restart callback
    pub async fn set_restart_callback(&self, callback: FlowEngineRestartCallback) {
        let mut cb = self.restart_callback.write().await;
        *cb = Some(callback);
    }

    /// Set the cancellation token
    pub async fn set_cancel_token(&self, token: CancellationToken) {
        let mut ct = self.cancel_token.write().await;
        *ct = Some(token);
    }

    /// Start event listeners and debug message handling
    pub async fn start_event_listeners(&self, cancel_token: tokio_util::sync::CancellationToken) {
        let engine_guard = self.engine.read().await;
        if let Some(engine) = engine_guard.as_ref() {
            // Start debug message listener
            let debug_rx = engine.debug_channel().subscribe();
            self.comms.start_debug_listener(debug_rx, cancel_token.clone()).await;

            // Start status message listener
            let status_rx = engine.status_channel().subscribe();
            self.comms.start_status_listener(status_rx, cancel_token.clone()).await;

            // Start Engine event listener
            let mut event_rx = engine.subscribe_events();
            let comms = self.comms.clone();
            let event_cancel_token = cancel_token.clone();

            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        result = event_rx.recv() => {
                            match result {
                                Ok(event) => {
                                    log::debug!("Received engine event: {event:?}");

                                    match event {
                                        EngineEvent::EngineStarted => {
                                            comms.send_notification("info", "Engine started").await;
                                        }
                                        EngineEvent::EngineStopped => {
                                            comms.send_notification("info", "Engine stopped").await;
                                        }
                                        EngineEvent::EngineRestartStarted => {
                                            comms.send_notification("info", "Engine restart started").await;
                                        }
                                        EngineEvent::EngineRestartCompleted => {
                                            comms.send_notification("success", "Engine restart completed").await;
                                        }
                                        EngineEvent::DebugChannelReinitialized => {
                                            comms
                                                .send_notification("info", "Debug channel reinitialized - please refresh debug panel")
                                                .await;
                                        }
                                        EngineEvent::FlowDeploymentStarted => {
                                            comms.send_notification("info", "Flow deployment started").await;
                                        }
                                        EngineEvent::FlowDeploymentCompleted => {
                                            comms.send_notification("success", "Flow deployment completed").await;
                                        }
                                        EngineEvent::Custom { event_type, .. } => {
                                            comms.send_notification("info", &format!("Custom event: {event_type}")).await;
                                        }
                                    }
                                }
                                Err(_) => {
                                    log::debug!("Engine event channel closed");
                                    break;
                                }
                            }
                        }
                        _ = event_cancel_token.cancelled() => {
                            // Cancellation signal received, wait a short time for Engine to send final events
                            log::debug!("Cancellation signal received, waiting for final engine events...");

                            // Give Engine some time to send EngineStopped event
                            let timeout = tokio::time::Duration::from_millis(1000); // 1 second timeout
                            let mut timeout_timer = tokio::time::interval(timeout);
                            timeout_timer.tick().await; // Skip the first immediate trigger

                            tokio::select! {
                                result = event_rx.recv() => {
                                    match result {
                                        Ok(EngineEvent::EngineStopped) => {
                                            log::debug!("Received final EngineStopped event");
                                            comms.send_notification("info", "Engine stopped").await;
                                            break;
                                        }
                                        Ok(event) => {
                                            log::debug!("Received final engine event: {event:?}");
                                            // Handle other events but continue waiting
                                        }
                                        Err(_) => {
                                            log::debug!("Engine event channel closed during shutdown");
                                            break;
                                        }
                                    }
                                }
                                _ = timeout_timer.tick() => {
                                    log::debug!("Timeout waiting for final engine events, shutting down event listener");
                                    break;
                                }
                            }
                            break;
                        }
                    }
                }
            });
        }
    }

    /// Deploy flows using Engine's redeploy_flows method
    pub async fn redeploy_flows(&self, flows: serde_json::Value) -> Result<(), n2link_core::N2linkError> {
        let engine_guard = self.engine.read().await;
        let registry_guard = self.registry.read().await;
        if let (Some(engine), Some(registry)) = (engine_guard.as_ref(), registry_guard.as_ref()) {
            engine.redeploy_flows(flows, registry, None).await
        } else {
            Err(n2link_core::N2linkError::invalid_operation("engine is not available"))
        }
    }

    /// Start debug message listener (connect to Engine's debug channel)
    pub async fn start_debug_listener(
        &self,
        engine: &n2link_core::runtime::engine::Engine,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        let debug_rx = engine.debug_channel().subscribe();
        self.comms.start_debug_listener(debug_rx, cancel_token).await;
    }
}
