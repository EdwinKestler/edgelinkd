use std::path::PathBuf;
use std::sync::Arc;

use axum::serve;
use axum::{Extension, Router};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;

use n2link_core::runtime::credential_storage::CredentialStore;
use n2link_core::runtime::egress::EgressPolicyHandle;
use n2link_core::runtime::registry::RegistryHandle;

use crate::api::create_all_routes;
use crate::handlers::auth::AdminAuth;
use crate::handlers::fleet::Fleet;
use crate::handlers::{FlowEngineRestartCallback, WebRuntimeServices, WebState};
use crate::models::*;
use crate::protection::{ApiProtection, protect_static_request};

pub struct WebServer {
    pub static_dir: PathBuf,
    pub state: Arc<WebState>,
}

impl WebServer {
    pub fn new(static_dir: impl Into<PathBuf>, cancel_token: CancellationToken, cfg: &config::Config) -> Self {
        let egress = EgressPolicyHandle::load(Some(cfg))
            .unwrap_or_else(|err| panic!("egress configuration is not valid: {err}"));
        Self::new_with_egress(static_dir, cancel_token, cfg, egress)
    }

    pub fn new_with_egress(
        static_dir: impl Into<PathBuf>,
        cancel_token: CancellationToken,
        cfg: &config::Config,
        egress: EgressPolicyHandle,
    ) -> Self {
        let mut args = match RedSystemSettings::load(cfg) {
            Ok(a) => a,
            Err(e) => {
                log::warn!("Failed to load WebServerArgs from config: {e}, using default");
                RedSystemSettings::default()
            }
        };
        let auth = AdminAuth::from_config_with_egress(cfg, egress.clone())
            .unwrap_or_else(|err| panic!("admin configuration is not valid: {err}"));
        let config_editor_enabled = cfg.get_bool("config_editor.enabled").unwrap_or(false);
        args.config_editor = config_editor_enabled;
        let args = Arc::new(args);
        assert!(!config_editor_enabled || auth.enabled(), "config editor requires configured admin authentication");
        let fleet = Fleet::from_config_with_egress(cfg, egress.clone())
            .unwrap_or_else(|err| panic!("fleet configuration is not valid: {err}"));
        let credentials = CredentialStore::from_config(Some(cfg))
            .unwrap_or_else(|err| panic!("credential storage configuration is not valid: {err}"));
        let protection = ApiProtection::load(Some(cfg))
            .unwrap_or_else(|err| panic!("api protection configuration is not valid: {err}"));
        let history = n2link_core::runtime::history::HistoryHandle::from_config(Some(cfg))
            .unwrap_or_else(|err| panic!("history configuration is not valid: {err}"));
        let web_state = WebState::assemble_with_egress(
            args,
            static_dir.into(),
            Some(cancel_token.clone()),
            auth,
            fleet,
            WebRuntimeServices {
                egress,
                credentials,
                protection,
                history,
                copilot_strict_metadata: cfg.get_bool("copilot.strict_metadata").unwrap_or(true),
            },
            config_editor_enabled,
        );

        // Start heartbeat task
        tokio::spawn({
            let comms = web_state.comms.clone();
            let cancel = cancel_token.clone();
            async move {
                comms.start_heartbeat_task(cancel).await;
            }
        });

        Self { static_dir: web_state.static_dir.clone(), state: web_state }
    }

    pub async fn with_registry(self, registry: RegistryHandle) -> Self {
        {
            let mut reg = self.state.registry.write().await;
            *reg = Some(registry);
        }
        self
    }

    #[cfg(feature = "nodes_wasm")]
    pub async fn with_plugin_store(
        self,
        store: Option<std::sync::Arc<n2link_core::runtime::wasm::PluginStore>>,
    ) -> Self {
        if let Some(store) = store {
            self.state.set_plugin_store(store).await;
        }
        self
    }

    pub async fn with_flows_file_path(self, path: PathBuf) -> Self {
        self.state.set_flows_file_path(path).await;
        self
    }

    pub async fn with_config_file_path(self, path: PathBuf) -> Self {
        self.state.set_config_file_path(path).await;
        self
    }

    pub async fn with_restart_callback(self, callback: FlowEngineRestartCallback) -> Self {
        {
            let mut cb = self.state.restart_callback.write().await;
            *cb = Some(callback);
        }
        self
    }

    pub async fn with_engine(
        self,
        engine: std::sync::Arc<tokio::sync::RwLock<n2link_core::runtime::engine::Engine>>,
        cancel_token: CancellationToken,
    ) -> Self {
        // Get the internal reference of Engine
        let engine_guard = engine.read().await;
        let engine_inner = engine_guard.clone();
        drop(engine_guard); // Release read lock

        {
            let mut eng = self.state.engine.write().await;
            *eng = Some(std::sync::Arc::new(engine_inner));
        }

        // Start event listeners and debug listeners
        self.state.start_event_listeners(cancel_token).await;

        self
    }

    pub fn router(&self) -> Router {
        // Create API routes (directly under root path, compatible with Node-RED frontend)
        let api_routes = create_all_routes(&self.state);

        // Static file service - use the static directory from the instance
        let static_service = ServeDir::new(&self.static_dir);

        // Use trait object for Extension so handlers using Extension<Arc<dyn WebStateCore + Send + Sync>> work
        Router::new()
            .merge(api_routes)
            .fallback_service(static_service)
            .layer(axum::middleware::from_fn(protect_static_request))
            .layer(Extension(self.state.clone()))
            .layer(Extension(
                self.state.clone() as Arc<dyn n2link_core::web::web_state_trait::WebStateCore + Send + Sync>
            ))
    }

    /// Start the web server and return a JoinHandle
    pub async fn spawn(
        self,
        addr: std::net::SocketAddr,
        cancel_token: CancellationToken,
    ) -> n2link_core::Result<tokio::task::JoinHandle<()>> {
        let listener = TcpListener::bind(&addr).await?;
        let bound = listener.local_addr()?;
        self.state.record_listen(bound).await;
        let router = self.router();
        let state = self.state.clone();
        Ok(tokio::spawn(async move {
            let shutdown = async move {
                cancel_token.cancelled().await;
                log::info!("Web server shutting down gracefully...");
            };
            if let Err(e) = serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>())
                .with_graceful_shutdown(shutdown)
                .await
            {
                log::error!("Web server error: {e}");
            }
            state.history.shutdown();
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[should_panic(expected = "config editor requires configured admin authentication")]
    async fn the_configuration_editor_cannot_run_without_authentication() {
        let cfg = config::Config::builder()
            .add_source(config::File::from_str("[config_editor]\nenabled = true\n", config::FileFormat::Toml))
            .build()
            .unwrap();

        let _ = WebServer::new(std::env::temp_dir(), CancellationToken::new(), &cfg);
    }
}
