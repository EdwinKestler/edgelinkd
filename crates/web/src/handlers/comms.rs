use axum::{
    Extension,
    extract::{ws::WebSocket, ws::WebSocketUpgrade},
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::sync::broadcast;
use tokio::time::{Duration, interval, sleep_until};

use crate::handlers::WebState;
use n2link_core::runtime::ingress::EndpointClass;

/// Node-RED WebSocket message format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRedMessage {
    pub topic: String,
    pub data: serde_json::Value,
}

/// Node-RED WebSocket message batch (array format)
pub type NodeRedMessageBatch = Vec<NodeRedMessage>;

/// WebSocket connection manager
#[derive(Debug, Clone)]
pub struct CommsManager {
    /// Broadcast channel sender
    pub broadcast_tx: broadcast::Sender<String>,
    /// Active connections
    pub connections: Arc<RwLock<HashMap<String, ConnectionInfo>>>,
}

/// Connection information
#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    /// Connection-specific sender
    pub tx: broadcast::Sender<String>,
    /// List of subscribed topics
    pub subscriptions: Arc<RwLock<HashSet<String>>>,
    /// Last activity time
    pub last_activity: Arc<RwLock<std::time::Instant>>,
    /// Session token this socket authenticated with. Never logged.
    pub token: Arc<RwLock<SocketAuth>>,
}

/// WebSocket session binding. `BoundInvalid` closes the idle loop immediately.
#[derive(Clone, Default)]
pub enum SocketAuth {
    #[default]
    Unbound,
    BoundValid {
        token: String,
        deadline: tokio::time::Instant,
    },
    BoundInvalid,
}

impl std::fmt::Debug for SocketAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unbound => f.write_str("Unbound"),
            Self::BoundValid { deadline, .. } => f.debug_struct("BoundValid").field("deadline", deadline).finish(),
            Self::BoundInvalid => f.write_str("BoundInvalid"),
        }
    }
}

#[derive(Clone, Copy)]
enum IdleWait {
    Park,
    Until(tokio::time::Instant),
    CloseNow,
}

fn idle_wait(auth: &SocketAuth, now: tokio::time::Instant) -> IdleWait {
    match auth {
        SocketAuth::Unbound => IdleWait::Park,
        SocketAuth::BoundValid { deadline, .. } if *deadline > now => IdleWait::Until(*deadline),
        SocketAuth::BoundValid { .. } | SocketAuth::BoundInvalid => IdleWait::CloseNow,
    }
}

async fn apply_revocation(
    result: Result<String, broadcast::error::RecvError>,
    token_slot: &RwLock<SocketAuth>,
    auth: &crate::handlers::auth::AdminAuth,
) -> bool {
    match result {
        Ok(revoked) => {
            let mut slot = token_slot.write().await;
            if let SocketAuth::BoundValid { token, .. } = &*slot
                && token == &revoked
            {
                *slot = SocketAuth::BoundInvalid;
                return true;
            }
            false
        }
        Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => {
            let mut slot = token_slot.write().await;
            match &*slot {
                SocketAuth::BoundValid { token, .. } if !auth.session_valid(token) => {
                    *slot = SocketAuth::BoundInvalid;
                    true
                }
                SocketAuth::BoundInvalid => true,
                _ => false,
            }
        }
    }
}

impl Default for CommsManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CommsManager {
    /// Start status message listener task
    pub async fn start_status_listener(
        &self,
        mut status_rx: tokio::sync::broadcast::Receiver<n2link_core::runtime::status_channel::StatusMessage>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        let comms_manager = self.clone();
        tokio::spawn(async move {
            log::info!("Status message listener started");
            loop {
                tokio::select! {
                    result = status_rx.recv() => {
                        match result {
                            Ok(status_msg) => {
                                let sender_id = format!("{}", status_msg.sender_id);
                                match serde_json::to_value(status_msg.status) {
                                    Ok(jv) => comms_manager.send_node_status_json(&sender_id, jv).await,
                                    Err(err) => {
                                        log::warn!("Failed to serialize status from {sender_id}: {err}");
                                    }
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                                log::warn!("Status message receiver lagged, skipped {skipped} messages");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                log::info!("Status message channel closed");
                                break;
                            }
                        }
                    }
                    _ = cancel_token.cancelled() => {
                        log::debug!("Status message listener received cancellation signal, waiting for final messages...");

                        // Give some time to receive the last status messages
                        let timeout = tokio::time::Duration::from_millis(500);
                        let mut timeout_timer = tokio::time::interval(timeout);
                        timeout_timer.tick().await; // Skip the first immediate trigger

                        tokio::select! {
                            result = status_rx.recv() => {
                                match result {
                                    Ok(status_msg) => {
                                        log::debug!("Received final status message: {status_msg:?}");
                                        let sender_id = format!("{}", status_msg.sender_id);
                                        match serde_json::to_value(status_msg.status) {
                                            Ok(jv) => comms_manager.send_node_status_json(&sender_id, jv).await,
                                            Err(err) => {
                                                log::warn!("Failed to serialize final status from {sender_id}: {err}");
                                            }
                                        }
                                    }
                                    Err(_) => {
                                        log::debug!("Status channel closed during shutdown");
                                    }
                                }
                            }
                            _ = timeout_timer.tick() => {
                                log::debug!("Timeout waiting for final status messages");
                            }
                        }
                        break;
                    }
                }
            }
        });
    }
    pub fn new() -> Self {
        let (broadcast_tx, _) = broadcast::channel(1000);
        Self { broadcast_tx, connections: Arc::new(RwLock::new(HashMap::new())) }
    }

    /// Create a single Node-RED message
    pub fn create_message(topic: &str, data: &serde_json::Value) -> NodeRedMessage {
        NodeRedMessage { topic: topic.to_string(), data: data.clone() }
    }

    /// Create a batch of Node-RED messages
    pub fn create_batch(messages: Vec<(String, serde_json::Value)>) -> NodeRedMessageBatch {
        messages.into_iter().map(|(topic, data)| NodeRedMessage { topic, data }).collect()
    }

    /// Serialize message batch to string
    pub fn serialize_batch(batch: &NodeRedMessageBatch) -> String {
        serde_json::to_string(batch).unwrap_or_else(|e| {
            log::error!("Failed to serialize message batch: {e}");
            "[]".to_string()
        })
    }

    /// Add new connection
    pub async fn add_connection(&self, id: String, tx: broadcast::Sender<String>) {
        let connection_info = ConnectionInfo {
            tx,
            subscriptions: Arc::new(RwLock::new(HashSet::new())),
            last_activity: Arc::new(RwLock::new(std::time::Instant::now())),
            token: Arc::new(RwLock::new(SocketAuth::Unbound)),
        };

        let mut connections = self.connections.write().await;
        connections.insert(id, connection_info);
    }

    /// Remove connection
    pub async fn remove_connection(&self, id: &str) {
        let mut connections = self.connections.write().await;
        connections.remove(id);
    }

    /// Update connection activity time
    pub async fn update_activity(&self, id: &str) {
        let connections = self.connections.read().await;
        if let Some(connection) = connections.get(id) {
            let mut last_activity = connection.last_activity.write().await;
            *last_activity = std::time::Instant::now();
        }
    }

    /// Subscribe to topic
    pub async fn subscribe(&self, connection_id: &str, topic: &str) {
        let connections = self.connections.read().await;
        if let Some(connection) = connections.get(connection_id) {
            let mut subscriptions = connection.subscriptions.write().await;
            subscriptions.insert(topic.to_string());
        }
    }

    /// Unsubscribe from topic
    pub async fn unsubscribe(&self, connection_id: &str, topic: &str) {
        let connections = self.connections.read().await;
        if let Some(connection) = connections.get(connection_id) {
            let mut subscriptions = connection.subscriptions.write().await;
            subscriptions.remove(topic);
        }
    }

    /// Broadcast message to all connections (legacy format, for compatibility)
    pub async fn broadcast(&self, message: &str) {
        let _ = self.broadcast_tx.send(message.to_string());
    }

    /// Send message to subscribers of a specific topic (Node-RED format)
    pub async fn send_to_topic(&self, topic: &str, data: &serde_json::Value) {
        let message = Self::create_message(topic, data);
        let batch = vec![message];
        let message_str = Self::serialize_batch(&batch);

        log::debug!("Sending message to topic '{topic}' bytes={}", message_str.len());

        let connections = self.connections.read().await;
        for connection in connections.values() {
            let subscriptions = connection.subscriptions.read().await;
            if subscriptions.contains(topic)
                || subscriptions.contains(&format!("{}/#", topic.split('/').next().unwrap_or("")))
            {
                let _ = connection.tx.send(message_str.clone());
            }
        }
    }

    /// Send message to subscribers of a specific topic (Node-RED format)
    pub async fn send_raw_json(&self, sub_topic: &str, rep_topic: &str, data: serde_json::Value) {
        let batch = vec![NodeRedMessage { topic: rep_topic.to_string(), data }];
        let message_str = Self::serialize_batch(&batch);
        log::debug!("Sending raw JSON message to topic '{rep_topic}' bytes={}", message_str.len());
        let connections = self.connections.read().await;
        for connection in connections.values() {
            let subscriptions = connection.subscriptions.read().await;
            if subscriptions.contains(sub_topic) {
                let _ = connection.tx.send(message_str.clone());
            }
        }
    }

    /// Send a single message to a connection
    pub async fn send_to_connection(&self, connection_id: &str, topic: &str, data: &serde_json::Value) {
        let message = Self::create_message(topic, data);
        let batch = vec![message];
        let message_str = Self::serialize_batch(&batch);

        let connections = self.connections.read().await;
        if let Some(connection) = connections.get(connection_id) {
            let _ = connection.tx.send(message_str);
        }
    }

    /// Send a batch of messages to subscribers of a specific topic
    pub async fn send_batch_to_topic(&self, topic: &str, batch: &NodeRedMessageBatch) {
        let message_str = Self::serialize_batch(batch);

        log::debug!("Sending batch to topic '{topic}' bytes={}", message_str.len());

        let connections = self.connections.read().await;
        for connection in connections.values() {
            let subscriptions = connection.subscriptions.read().await;
            if subscriptions.contains(topic)
                || subscriptions.contains(&format!("{}/#", topic.split('/').next().unwrap_or("")))
            {
                let _ = connection.tx.send(message_str.clone());
            }
        }
    }

    /// Broadcast message to all connections (Node-RED format)
    pub async fn broadcast_message(&self, topic: &str, data: &serde_json::Value) {
        let message = Self::create_message(topic, data);
        let batch = vec![message];
        let message_str = Self::serialize_batch(&batch);

        log::debug!("Broadcasting message to all connections bytes={}", message_str.len());

        let _ = self.broadcast_tx.send(message_str);
    }

    /// Broadcast a batch of messages to all connections
    pub async fn broadcast_batch(&self, batch: &NodeRedMessageBatch) {
        let message_str = Self::serialize_batch(batch);
        let _ = self.broadcast_tx.send(message_str);
    }

    /// Send heartbeat message to all connections
    pub async fn send_heartbeat(&self) {
        let heartbeat_data = serde_json::json!(chrono::Utc::now().timestamp_millis());
        self.broadcast_message("hb", &heartbeat_data).await;
    }
    /// Start heartbeat task
    pub async fn start_heartbeat_task(&self, cancel_token: tokio_util::sync::CancellationToken) {
        let comms_manager = self.clone();
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(15)); // Send heartbeat every 15 seconds

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        comms_manager.send_heartbeat().await;
                    }
                    _ = cancel_token.cancelled() => {
                        log::debug!("UI Heartbeat task shutting down...");
                        break;
                    }
                }
            }
        });
    }

    /// Start debug message listener task
    pub async fn start_debug_listener(
        &self,
        debug_rx: tokio::sync::broadcast::Receiver<n2link_core::runtime::debug_channel::DebugMessage>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        let comms_manager = self.clone();
        tokio::spawn(async move {
            let mut debug_rx = debug_rx;
            log::info!("Debug message listener started");

            loop {
                tokio::select! {
                    result = debug_rx.recv() => {
                        match result {
                            Ok(debug_msg) => {
                                log::debug!("Received debug message from channel: {debug_msg:?}");

                                // Construct Node-RED compatible message format
                                let node_red_data = serde_json::json!({
                                    "id": debug_msg.id,
                                    "z": debug_msg.path,
                                    "path": debug_msg.path,
                                    "name": debug_msg.name.unwrap_or_default(),
                                    "topic": debug_msg.topic.unwrap_or_default(),
                                    "property": debug_msg.property.unwrap_or_default(),
                                    "msg": debug_msg.msg,
                                    "format": debug_msg.format.unwrap_or_default()
                                });

                                comms_manager.send_to_topic("debug", &node_red_data).await;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                                log::warn!("Debug message receiver lagged, skipped {skipped} messages");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                log::info!("Debug message channel closed");
                                break;
                            }
                        }
                    }
                    _ = cancel_token.cancelled() => {
                        log::debug!("Debug message listener received cancellation signal, waiting for final messages...");

                        // Give some time to receive the last debug messages
                        let timeout = tokio::time::Duration::from_millis(500);
                        let mut timeout_timer = tokio::time::interval(timeout);
                        timeout_timer.tick().await; // Skip the first immediate trigger

                        tokio::select! {
                            result = debug_rx.recv() => {
                                match result {
                                    Ok(debug_msg) => {
                                        log::debug!("Received final debug message: {debug_msg:?}");
                                        let node_red_data = serde_json::json!({
                                            "id": debug_msg.id,
                                            "z": debug_msg.path,
                                            "path": debug_msg.path,
                                            "name": debug_msg.name.unwrap_or_default(),
                                            "topic": debug_msg.topic.unwrap_or_default(),
                                            "property": debug_msg.property.unwrap_or_default(),
                                            "msg": debug_msg.msg,
                                            "format": debug_msg.format.unwrap_or_default()
                                        });
                                        comms_manager.send_to_topic("debug", &node_red_data).await;
                                    }
                                    Err(_) => {
                                        log::debug!("Debug channel closed during shutdown");
                                    }
                                }
                            }
                            _ = timeout_timer.tick() => {
                                log::debug!("Timeout waiting for final debug messages");
                            }
                        }
                        break;
                    }
                }
            }
        });
    }

    /// Send debug message
    pub async fn send_debug_message(&self, node_id: &str, node_name: &str, msg_data: serde_json::Value) {
        let debug_data = serde_json::json!({
            "id": node_id,
            "name": node_name,
            "msg": msg_data,
            "timestamp": chrono::Utc::now().timestamp_millis(),
            "_msgid": uuid::Uuid::new_v4().to_string()
        });

        self.send_to_topic("debug", &debug_data).await;
    }

    /// Send node status update
    pub async fn send_node_status_json(&self, node_id: &str, status_json: serde_json::Value) {
        self.send_raw_json("status/#", &format!("status/{node_id}"), status_json).await;
    }

    /// Send notification message
    pub async fn send_notification(&self, level: &str, text: &str) {
        let notification_data = serde_json::json!({
            "level": level,
            "text": text,
            "timestamp": chrono::Utc::now().timestamp_millis()
        });

        self.send_to_topic("notification/runtime", &notification_data).await;
    }

    /// Send deploy notification
    pub async fn send_deploy_notification(&self, _success: bool, revision: Option<&str>) {
        let deploy_data = serde_json::json!({
            "revision": revision,
        });

        self.send_to_topic("notification/runtime-deploy", &deploy_data).await;
    }

    /// Send runtime deploy initial data
    pub async fn send_runtime_deploy_initial(
        &self,
        connection_id: &str,
        engine: Option<&n2link_core::runtime::engine::Engine>,
    ) {
        let revision = if let Some(engine) = engine { Some(engine.flows_rev().await) } else { None };

        let deploy_data = serde_json::json!({
            "revision": revision
        });

        // Send runtime state and deploy info as batch
        let batch = vec![
            Self::create_message(
                "notification/runtime-state",
                &serde_json::json!({
                    "state": "start",
                    "deploy": true
                }),
            ),
            Self::create_message("notification/runtime-deploy", &deploy_data),
        ];

        self.send_to_connection(connection_id, "batch", &serde_json::json!(batch)).await;
    }
}

/// WebSocket upgrade handler
pub async fn websocket_handler(ws: WebSocketUpgrade, Extension(state): Extension<Arc<WebState>>) -> Response {
    let max_message_size = state.protection.limits(EndpointClass::Websocket).max_body_bytes;
    let permit = match state.protection.acquire_websocket().await {
        Ok(permit) => permit,
        Err(response) => return *response,
    };
    ws.max_message_size(max_message_size).on_upgrade(move |socket| async move {
        let _permit = permit;
        handle_websocket(socket, Arc::clone(&state)).await;
    })
}

/// Handle WebSocket connection
async fn handle_websocket(socket: WebSocket, state: Arc<WebState>) {
    let connection_id = uuid::Uuid::new_v4().to_string();
    log::info!("New WebSocket connection: {connection_id}");

    // Create connection-specific broadcast channel
    let (tx, mut rx) = broadcast::channel::<String>(100);

    // Add connection to manager
    state.comms.add_connection(connection_id.clone(), tx.clone()).await;

    // Split WebSocket sender and receiver
    let (mut sender, mut receiver) = socket.split();

    // Get cancellation token (lock and clone Option)
    let cancel_token = {
        let guard = state.cancel_token.read().await;
        guard.clone()
    };

    let conn_cancel = tokio_util::sync::CancellationToken::new();
    let conn_cancel_broadcast = conn_cancel.clone();

    // Handle broadcast messages in background task
    let tx_clone = tx.clone();
    let connection_id_clone = connection_id.clone();
    let broadcast_cancel_token = cancel_token.clone();
    let mut broadcast_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                msg_result = rx.recv() => {
                    match msg_result {
                        Ok(msg) => {
                            if let Err(e) = sender.send(axum::extract::ws::Message::Text(msg.into())).await {
                                log::error!("Failed to send WebSocket message: {e}");
                                break;
                            }
                        }
                        Err(_) => {
                            log::debug!("WebSocket broadcast channel closed for connection: {connection_id_clone}");
                            break;
                        }
                    }
                }
                _ = conn_cancel_broadcast.cancelled() => {
                    log::info!("WebSocket session closed connection={connection_id_clone}");
                    let _ = sender.send(axum::extract::ws::Message::Close(None)).await;
                    break;
                }
                _ = async {
                    if let Some(ref token) = broadcast_cancel_token {
                        token.cancelled().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    log::info!("WebSocket broadcast task cancelled for connection: {connection_id_clone}");
                    let _ = sender.send(axum::extract::ws::Message::Close(None)).await;
                    break;
                }
            }
        }
        log::info!("WebSocket broadcast task ended for connection: {connection_id_clone}");
    });

    // Handle client messages. When admin auth is off the socket is already authorized, which is
    // what the editor does: it ignores the welcome "required" unless it has a token.
    let connection_id_clone = connection_id.clone();
    let comms_manager_clone = state.comms.clone();
    let message_cancel_token = cancel_token.clone();
    let state2 = Arc::clone(&state);
    let auth = Arc::clone(&state.auth);
    let authed = Arc::new(std::sync::atomic::AtomicBool::new(!state.auth.enabled()));
    let token_slot = {
        let connections = state.comms.connections.read().await;
        connections
            .get(&connection_id)
            .map(|info| info.token.clone())
            .unwrap_or_else(|| Arc::new(RwLock::new(SocketAuth::Unbound)))
    };
    let mut revocations = state.auth.subscribe_revocations();
    let conn_cancel_msg = conn_cancel.clone();
    let mut message_task = tokio::spawn(async move {
        loop {
            let wait = {
                let mut slot = token_slot.write().await;
                if let SocketAuth::BoundValid { token, .. } = &*slot
                    && !auth.session_valid(token)
                {
                    *slot = SocketAuth::BoundInvalid;
                }
                idle_wait(&slot, tokio::time::Instant::now())
            };
            tokio::select! {
                msg_option = receiver.next() => {
                    match msg_option {
                        Some(Ok(axum::extract::ws::Message::Text(text))) => {
                            let bytes = text.len();
                            comms_manager_clone.update_activity(&connection_id_clone).await;
                            match serde_json::from_str::<Value>(&text) {
                                Ok(parsed) => {
                                    let category = message_category(&parsed);
                                    log::debug!("websocket recv connection={connection_id_clone} category={category} bytes={bytes}");
                                    let engine_guard = state2.engine.read().await;
                                    handle_websocket_message(
                                        parsed,
                                        &tx_clone,
                                        &connection_id_clone,
                                        &comms_manager_clone,
                                        engine_guard.as_deref(),
                                        &auth,
                                        &authed,
                                        &token_slot,
                                    )
                                    .await;
                                    if matches!(*token_slot.read().await, SocketAuth::BoundInvalid) {
                                        log::info!("WebSocket session invalid connection={connection_id_clone}");
                                        break;
                                    }
                                }
                                Err(_) => {
                                    log::debug!("websocket recv connection={connection_id_clone} category=malformed bytes={bytes}");
                                }
                            }
                        }
                        Some(Ok(axum::extract::ws::Message::Pong(_))) => {
                            comms_manager_clone.update_activity(&connection_id_clone).await;
                        }
                        Some(Ok(axum::extract::ws::Message::Close(_))) => {
                            log::info!("WebSocket connection closed: {connection_id_clone}");
                            break;
                        }
                        Some(Err(e)) => {
                            log::error!("WebSocket error connection={connection_id_clone}: {e}");
                            break;
                        }
                        Some(_) => {}
                        None => {
                            log::info!("WebSocket receiver stream ended for connection: {connection_id_clone}");
                            break;
                        }
                    }
                }
                revoked = revocations.recv() => {
                    if apply_revocation(revoked, &token_slot, &auth).await {
                        log::info!("WebSocket session revoked connection={connection_id_clone}");
                        break;
                    }
                }
                _ = async {
                    match wait {
                        IdleWait::Until(at) => sleep_until(at).await,
                        IdleWait::CloseNow => {}
                        IdleWait::Park => std::future::pending::<()>().await,
                    }
                } => {
                    *token_slot.write().await = SocketAuth::BoundInvalid;
                    log::info!("WebSocket session expired connection={connection_id_clone}");
                    break;
                }
                _ = async {
                    if let Some(ref token) = message_cancel_token {
                        token.cancelled().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    log::info!("WebSocket message task cancelled for connection: {connection_id_clone}");
                    break;
                }
            }
        }
        conn_cancel_msg.cancel();
        log::info!("WebSocket message task ended for connection: {connection_id_clone}");
    });

    // Send initial connection confirmation (Node-RED auth format)
    let welcome_msg = serde_json::json!({
        "auth": "required",
        "version": "5.0.7"
    });

    if let Err(e) = tx.send(welcome_msg.to_string()) {
        log::error!("Failed to send welcome message: {e}");
    }

    tokio::select! {
        _ = &mut broadcast_task => {
            message_task.abort();
        }
        _ = &mut message_task => {
            conn_cancel.cancel();
            let _ = tokio::time::timeout(Duration::from_millis(500), &mut broadcast_task).await;
            broadcast_task.abort();
        }
    }

    // Remove connection
    state.comms.remove_connection(&connection_id).await;
    log::info!("WebSocket connection ended: {connection_id}");
}

fn message_category(message: &Value) -> &'static str {
    if message.get("auth").is_some() {
        "auth"
    } else if message.get("subscribe").is_some() {
        "subscribe"
    } else if message.get("unsubscribe").is_some() {
        "unsubscribe"
    } else {
        "other"
    }
}

/// Handle WebSocket message
#[allow(clippy::too_many_arguments)]
async fn handle_websocket_message(
    message: Value,
    tx: &broadcast::Sender<String>,
    connection_id: &str,
    comms_manager: &CommsManager,
    engine: Option<&n2link_core::runtime::engine::Engine>,
    auth: &crate::handlers::auth::AdminAuth,
    authed: &std::sync::atomic::AtomicBool,
    token_slot: &RwLock<SocketAuth>,
) {
    if message.get("auth").is_some() {
        let offered = message.get("auth").and_then(Value::as_str);
        let accepted = if !auth.enabled() { true } else { offered.is_some_and(|token| auth.session_valid(token)) };
        if accepted {
            authed.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(token) = offered {
                let next = if let Some(deadline) = auth.session_expires(token) {
                    SocketAuth::BoundValid { token: token.to_string(), deadline }
                } else {
                    SocketAuth::BoundInvalid
                };
                *token_slot.write().await = next;
            }
            log::debug!("websocket auth connection={connection_id} result=ok");
        } else {
            log::debug!("websocket auth connection={connection_id} result=fail");
        }
        let response =
            if accepted { serde_json::json!({ "auth": "ok" }) } else { serde_json::json!({ "auth": "fail" }) };
        if let Err(err) = tx.send(response.to_string()) {
            log::error!("Failed to send auth response connection={connection_id}: {err}");
        }
    }

    let bound = token_slot.read().await;
    if auth.enabled() {
        let still = match &*bound {
            SocketAuth::BoundValid { token, .. } => auth.session_valid(token),
            SocketAuth::BoundInvalid => false,
            SocketAuth::Unbound => false,
        };
        if !still {
            return;
        }
    } else if !authed.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    drop(bound);

    // Handle subscribe message
    if let Some(topic) = message.get("subscribe").and_then(|t| t.as_str()) {
        log::info!("Client subscribed to topic: {topic}");

        // Add to connection's subscription list
        comms_manager.subscribe(connection_id, topic).await;

        // Send initial data according to the subscribed topic (Node-RED compatible format)
        match topic {
            "debug" => {
                // Debug topic doesn't send initial data
            }
            topic if topic.starts_with("status/") => {
                // Send status info initialization
                let status_init_data = serde_json::json!({
                    "type": "status",
                    "status": {}
                });
                let status_batch = vec![CommsManager::create_message(topic, &status_init_data)];
                let _ = tx.send(CommsManager::serialize_batch(&status_batch));
            }
            "notification/#" => {
                // Send runtime deploy notification with revision
                let revision = if let Some(engine) = engine { Some(engine.flows_rev().await) } else { None };
                let deploy_data = serde_json::json!({
                    "revision": revision
                });

                // Send runtime state and deploy info as batch
                let batch = vec![
                    CommsManager::create_message(
                        "notification/runtime-state",
                        &serde_json::json!({
                            "state": "start",
                            "deploy": true
                        }),
                    ),
                    CommsManager::create_message("notification/runtime-deploy", &deploy_data),
                ];
                let _ = tx.send(CommsManager::serialize_batch(&batch));
            }
            "notification/runtime-deploy" => {
                // Send deploy notification for notification/# wildcard
                let revision = if let Some(engine) = engine { Some(engine.flows_rev().await) } else { None };
                // Send deploy notification (success = true)
                comms_manager.send_deploy_notification(true, revision.as_deref()).await;
            }
            topic if topic.starts_with("notification/") => {
                // Other notification topics - no initial data needed
            }
            _ => {}
        }
    }

    // Handle unsubscribe message
    if let Some(topic) = message.get("unsubscribe").and_then(|t| t.as_str()) {
        log::info!("Client unsubscribed from topic: {topic}");

        // Remove from connection's subscription list
        comms_manager.unsubscribe(connection_id, topic).await;

        // Node-RED doesn't send unsubscribe confirmation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::auth::AdminAuth;
    use serde_json::json;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Mutex, Once, OnceLock};

    static LOGS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

    struct Capture;
    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            LOGS.get_or_init(|| Mutex::new(Vec::new())).lock().unwrap().push(format!("{}", record.args()));
        }
        fn flush(&self) {}
    }

    fn install_logger() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let _ = log::set_logger(&Capture);
            log::set_max_level(log::LevelFilter::Debug);
        });
    }

    fn logs_contain(secret: &str) -> bool {
        LOGS.get().is_some_and(|logs| logs.lock().unwrap().iter().any(|line| line.contains(secret)))
    }

    fn auth() -> AdminAuth {
        AdminAuth::from_config(
            &config::Config::builder()
                .add_source(config::File::from_str(
                    r#"
            [admin]
            password = "plant-secret"
            "#,
                    config::FileFormat::Toml,
                ))
                .build()
                .unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn websocket_auth_does_not_log_the_token() {
        install_logger();
        let auth = auth();
        let issued = auth.issue("admin", "*");
        let secret = issued.token().to_string();
        let comms = CommsManager::new();
        let (tx, _rx) = broadcast::channel(8);
        comms.add_connection("c1".into(), tx.clone()).await;
        let token_slot = RwLock::new(SocketAuth::Unbound);
        let authed = AtomicBool::new(false);
        handle_websocket_message(json!({ "auth": secret }), &tx, "c1", &comms, None, &auth, &authed, &token_slot).await;
        handle_websocket_message(
            json!({ "auth": "not-a-real-token" }),
            &tx,
            "c1",
            &comms,
            None,
            &auth,
            &authed,
            &token_slot,
        )
        .await;
        assert!(!logs_contain(&secret));
        assert!(authed.load(std::sync::atomic::Ordering::Relaxed));
        handle_websocket_message(json!({ "subscribe": "debug" }), &tx, "c1", &comms, None, &auth, &authed, &token_slot)
            .await;
        {
            let subs = comms.connections.read().await.get("c1").unwrap().subscriptions.read().await.clone();
            assert!(subs.contains("debug"));
        }
        auth.revoke(&secret);
        assert!(!auth.session_valid(&secret));
        handle_websocket_message(
            json!({ "subscribe": "status/#" }),
            &tx,
            "c1",
            &comms,
            None,
            &auth,
            &authed,
            &token_slot,
        )
        .await;
        let subs = comms.connections.read().await.get("c1").unwrap().subscriptions.read().await.clone();
        assert!(!subs.contains("status/#"));
        assert!(!logs_contain(&secret));
    }

    #[tokio::test]
    async fn an_expired_session_is_no_longer_valid() {
        let auth = auth();
        let issued = auth.issue_with_ttl("admin", "*", Duration::from_millis(1));
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(!auth.session_valid(issued.token()));
        assert!(!logs_contain(issued.token()));
    }

    #[tokio::test]
    async fn a_different_token_is_not_revoked() {
        let auth = auth();
        let first = auth.issue("admin", "*");
        let second = auth.issue("admin", "*");
        auth.revoke(first.token());
        assert!(!auth.session_valid(first.token()));
        assert!(auth.session_valid(second.token()));
    }

    #[test]
    fn bound_invalid_closes_immediately() {
        let now = tokio::time::Instant::now();
        assert!(matches!(idle_wait(&SocketAuth::Unbound, now), IdleWait::Park));
        assert!(matches!(idle_wait(&SocketAuth::BoundInvalid, now), IdleWait::CloseNow));
        let past = now.checked_sub(Duration::from_secs(1)).unwrap_or(now);
        assert!(matches!(
            idle_wait(&SocketAuth::BoundValid { token: "x".into(), deadline: past }, now),
            IdleWait::CloseNow
        ));
    }

    #[tokio::test]
    async fn lagged_revocation_closes_an_invalid_session() {
        let auth = auth();
        let issued = auth.issue("admin", "*");
        let slot = RwLock::new(SocketAuth::BoundValid {
            token: issued.token().to_string(),
            deadline: tokio::time::Instant::now() + Duration::from_secs(60),
        });
        auth.revoke(issued.token());
        assert!(apply_revocation(Err(broadcast::error::RecvError::Lagged(8)), &slot, &auth).await);
        assert!(matches!(*slot.read().await, SocketAuth::BoundInvalid));
    }

    async fn serve_comms(state: Arc<crate::handlers::WebState>) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::api::create_all_routes(&state).layer(Extension(state));
        let task = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service()).await.unwrap();
        });
        (addr, task)
    }

    #[tokio::test]
    async fn a_revoked_websocket_is_closed() {
        let admin = auth();
        let issued = admin.issue("admin", "*");
        let token = issued.token().to_string();
        let state = crate::handlers::WebState::assemble(
            Arc::new(crate::models::RedSystemSettings::default()),
            std::env::temp_dir(),
            None,
            admin,
            crate::handlers::fleet::Fleet::disabled(),
        );
        let (addr, server) = serve_comms(state.clone()).await;
        let url = format!("ws://{addr}/comms");
        let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        use futures_util::{SinkExt, StreamExt};
        let welcome = socket.next().await.unwrap().unwrap();
        assert!(welcome.to_string().contains("required"), "{welcome:?}");
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(json!({ "auth": token }).to_string().into()))
            .await
            .unwrap();
        let ok = socket.next().await.unwrap().unwrap();
        assert!(ok.to_string().contains("ok"), "{ok:?}");
        state.auth.revoke(&token);
        let closed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match socket.next().await {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => return,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => return,
                }
            }
        })
        .await;
        assert!(closed.is_ok(), "revoked websocket stayed open");
        server.abort();
    }
}
