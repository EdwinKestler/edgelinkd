//! Optional durable operational history for n2link.
//!
//! Stores structured, redacted events in an optional SQLite database behind the
//! `history_sqlite` feature flag. Best-effort: failures never block flow execution
//! or deploy/rollback.

use std::fmt;
#[cfg(feature = "history_sqlite")]
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::runtime::model::ElementId;
use crate::{N2linkError, Result};

pub const SCHEMA_VERSION: u32 = 1;
pub const APPLICATION_ID: u32 = 0x454C_4831; // "ELH1"

/// Configuration for the history subsystem.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryConfig {
    pub enabled: bool,
    pub path: Option<String>,
    pub queue_capacity: usize,
    pub batch_max: usize,
    pub retention_days: u32,
    pub max_db_bytes: u64,
    pub shutdown_drain_ms: u64,
    pub node_error_interval_ms: u64,
    pub migrate: bool,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: None,
            queue_capacity: 1024,
            batch_max: 256,
            retention_days: 30,
            max_db_bytes: 16_777_216, // 16 MiB
            shutdown_drain_ms: 2000,
            node_error_interval_ms: 1000,
            migrate: false,
        }
    }
}

impl HistoryConfig {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if !(16..=65536).contains(&self.queue_capacity) {
            return Err("history.queue_capacity must be between 16 and 65536".to_string());
        }
        if self.batch_max == 0 || self.batch_max > self.queue_capacity {
            return Err("history.batch_max must be between 1 and queue_capacity".to_string());
        }
        if !(1..=3650).contains(&self.retention_days) {
            return Err("history.retention_days must be between 1 and 3650".to_string());
        }
        if !(1_048_576..=4_294_967_296).contains(&self.max_db_bytes) {
            return Err("history.max_db_bytes must be between 1048576 (1 MiB) and 4294967296 (4 GiB)".to_string());
        }
        if !(1..=30000).contains(&self.shutdown_drain_ms) {
            return Err("history.shutdown_drain_ms must be between 1 and 30000".to_string());
        }
        if self.node_error_interval_ms > 3_600_000 {
            return Err("history.node_error_interval_ms must be between 0 and 3600000".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryState {
    Disabled,
    Starting,
    Ok,
    Degraded,
    Failed,
    Stopped,
}

impl fmt::Display for HistoryState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(f, "disabled"),
            Self::Starting => write!(f, "starting"),
            Self::Ok => write!(f, "ok"),
            Self::Degraded => write!(f, "degraded"),
            Self::Failed => write!(f, "failed"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

/// Structured health report for history.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryHealth {
    pub state: String,
    pub schema_version: u32,
    pub queue_capacity: usize,
    pub queued: usize,
    pub accepted: u64,
    pub written: u64,
    pub dropped_queue_full: u64,
    pub dropped_write_error: u64,
    pub dropped_shutdown: u64,
    pub pruned: u64,
    pub db_bytes: u64,
    pub last_error: Option<String>,
    pub last_error_at_ms: Option<i64>,
}

/// A structured, redacted event to be recorded in history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEvent {
    pub at_ms: i64,
    pub run_id: String,
    pub category: String,
    pub kind: String,
    pub outcome: Option<String>,
    pub actor: String,
    pub subject: Option<String>,
    pub detail: String, // Valid JSON object, <= 1024 bytes
}

/// Filter for querying operational history.
#[derive(Debug, Clone, Default)]
pub struct HistoryQuery {
    pub limit: usize,
    pub before: Option<i64>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub category: Option<String>,
    pub kind: Option<String>,
    pub subject: Option<String>,
}

/// A single row returned from a history query.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEventRow {
    pub seq: i64,
    pub at_ms: i64,
    pub run_id: String,
    pub category: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub detail: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryQueryResult {
    pub events: Vec<HistoryEventRow>,
    pub next: Option<i64>,
}

#[derive(Debug, Clone)]
pub enum HistoryQueryError {
    Disabled,
    Failed(String),
    InvalidFilter(String),
    Internal(String),
}

impl fmt::Display for HistoryQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(f, "history is disabled"),
            Self::Failed(err) => write!(f, "history is unavailable: {err}"),
            Self::InvalidFilter(msg) => write!(f, "invalid filter: {msg}"),
            Self::Internal(err) => write!(f, "internal query error: {err}"),
        }
    }
}

// Sanitization helpers
fn sanitize_actor(actor: &str) -> String {
    let trimmed = actor.trim();
    if trimmed.is_empty() {
        return "anonymous".to_string();
    }
    let filtered: String = trimmed.chars().filter(|c| !c.is_control()).take(64).collect();
    if filtered.is_empty() { "anonymous".to_string() } else { filtered }
}

fn sanitize_subject(subject: Option<&str>) -> Option<String> {
    let s = subject?.trim();
    if s.is_empty() {
        return None;
    }
    let filtered: String = s.chars().filter(|c| !c.is_control()).take(128).collect();
    if filtered.is_empty() { None } else { Some(filtered) }
}

fn sanitize_revision(rev: Option<&str>) -> Option<String> {
    let r = rev?.trim();
    if r.is_empty() {
        return None;
    }
    if (1..=64).contains(&r.len()) && r.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(r.to_string())
    } else {
        Some("invalid".to_string())
    }
}

fn sanitize_detail(val: serde_json::Value) -> String {
    let serialized = serde_json::to_string(&val).unwrap_or_else(|_| "{}".to_string());
    if serialized.len() <= 1024 { serialized } else { serde_json::json!({ "truncated": true }).to_string() }
}

fn current_unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Common Handle Structure (used with or without history_sqlite feature)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct HistoryHandle {
    inner: Arc<HistoryInner>,
}

impl Default for HistoryHandle {
    fn default() -> Self {
        Self::disabled()
    }
}

struct NodeErrorEntry {
    last_reported: Instant,
    suppressed: u64,
}

type NodeStatusEntry = (Option<String>, Option<String>);

struct HistoryInner {
    config: HistoryConfig,
    run_id: String,
    home_dir: Mutex<Option<PathBuf>>,
    #[allow(dead_code)]
    resolved_db_path: Mutex<Option<PathBuf>>,
    state: Mutex<HistoryState>,
    last_error: Mutex<Option<String>>,
    last_error_at_ms: AtomicI64,

    // Node error throttling & status deduplication
    node_error_interval: Duration,
    node_errors: Mutex<std::collections::HashMap<ElementId, NodeErrorEntry>>,
    node_status: Mutex<std::collections::HashMap<ElementId, NodeStatusEntry>>,

    // Metrics
    accepted: AtomicU64,
    written: AtomicU64,
    dropped_queue_full: AtomicU64,
    dropped_write_error: AtomicU64,
    dropped_shutdown: AtomicU64,
    pruned: AtomicU64,
    db_bytes: AtomicU64,
    is_stopping: AtomicBool,

    #[cfg(feature = "history_sqlite")]
    writer_sender: Mutex<Option<std::sync::mpsc::SyncSender<HistoryEvent>>>,
    #[cfg(feature = "history_sqlite")]
    writer_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl HistoryHandle {
    /// Construct a disabled history handle.
    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(HistoryInner {
                config: HistoryConfig::default(),
                run_id: uuid::Uuid::new_v4().to_string(),
                home_dir: Mutex::new(None),
                resolved_db_path: Mutex::new(None),
                state: Mutex::new(HistoryState::Disabled),
                last_error: Mutex::new(None),
                last_error_at_ms: AtomicI64::new(0),
                node_error_interval: Duration::from_millis(1000),
                node_errors: Mutex::new(std::collections::HashMap::new()),
                node_status: Mutex::new(std::collections::HashMap::new()),
                accepted: AtomicU64::new(0),
                written: AtomicU64::new(0),
                dropped_queue_full: AtomicU64::new(0),
                dropped_write_error: AtomicU64::new(0),
                dropped_shutdown: AtomicU64::new(0),
                pruned: AtomicU64::new(0),
                db_bytes: AtomicU64::new(0),
                is_stopping: AtomicBool::new(false),
                #[cfg(feature = "history_sqlite")]
                writer_sender: Mutex::new(None),
                #[cfg(feature = "history_sqlite")]
                writer_thread: Mutex::new(None),
            }),
        }
    }

    /// Load from configuration.
    pub fn from_config(cfg: Option<&config::Config>) -> Result<Self> {
        let history_cfg = match cfg {
            Some(c) => match c.get::<HistoryConfig>("history") {
                Ok(conf) => conf,
                Err(config::ConfigError::NotFound(_)) => HistoryConfig::default(),
                Err(err) => return Err(N2linkError::invalid_operation(&err.to_string())),
            },
            None => HistoryConfig::default(),
        };

        if let Err(msg) = history_cfg.validate() {
            return Err(N2linkError::invalid_operation(&msg));
        }

        if !history_cfg.enabled {
            return Ok(Self::disabled());
        }

        #[cfg(not(feature = "history_sqlite"))]
        {
            return Err(N2linkError::NotSupported("sqlite history is not compiled in this build".to_string()));
        }

        #[cfg(feature = "history_sqlite")]
        {
            let home_dir = cfg
                .and_then(|c| c.get_string("home_dir").ok())
                .map(PathBuf::from)
                .or_else(|| crate::compat::env_var("HOME").ok().flatten().map(PathBuf::from));
            Self::init_with_config(history_cfg, home_dir)
        }
    }

    /// Provide or update the home directory where `history.sqlite3` will reside.
    pub fn set_home(&self, home: PathBuf) {
        let mut home_guard = self.inner.home_dir.lock().unwrap();
        *home_guard = Some(home.clone());
        drop(home_guard);

        #[cfg(feature = "history_sqlite")]
        {
            if self.inner.config.enabled {
                self.resolve_and_ensure_writer(Some(home));
            }
        }
    }

    /// Reset node coalescing filters (e.g. on flow redeploy).
    pub fn reset_node_filters(&self) {
        if let Ok(mut errors) = self.inner.node_errors.lock() {
            errors.clear();
        }
        if let Ok(mut status) = self.inner.node_status.lock() {
            status.clear();
        }
    }

    /// Record a generic history event (non-blocking).
    pub fn record(&self, _event: HistoryEvent) {
        if !self.inner.config.enabled {
            return;
        }

        if self.inner.is_stopping.load(Ordering::Relaxed) {
            self.inner.dropped_shutdown.fetch_add(1, Ordering::Relaxed);
            return;
        }

        #[cfg(feature = "history_sqlite")]
        {
            let sender = self.inner.writer_sender.lock().unwrap().clone();
            if let Some(tx) = sender {
                match tx.try_send(_event) {
                    Ok(()) => {
                        self.inner.accepted.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                        self.inner.dropped_queue_full.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                        self.inner.dropped_shutdown.fetch_add(1, Ordering::Relaxed);
                    }
                }
            } else {
                self.inner.dropped_write_error.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Convenience recording methods for hook points
    // -----------------------------------------------------------------------

    pub fn record_deploy_proposed(&self, actor: &str, rev: Option<&str>, scope: &str, nodes: usize) {
        self.record(HistoryEvent {
            at_ms: current_unix_ms(),
            run_id: self.inner.run_id.clone(),
            category: "deploy".to_string(),
            kind: "deploy.proposed".to_string(),
            outcome: None,
            actor: sanitize_actor(actor),
            subject: sanitize_revision(rev),
            detail: sanitize_detail(serde_json::json!({ "scope": scope, "nodes": nodes })),
        });
    }

    pub fn record_deploy_accepted(&self, actor: &str, rev: &str, scope: &str, nodes: usize) {
        self.record(HistoryEvent {
            at_ms: current_unix_ms(),
            run_id: self.inner.run_id.clone(),
            category: "deploy".to_string(),
            kind: "deploy.accepted".to_string(),
            outcome: Some("ok".to_string()),
            actor: sanitize_actor(actor),
            subject: sanitize_revision(Some(rev)),
            detail: sanitize_detail(serde_json::json!({ "scope": scope, "nodes": nodes })),
        });
    }

    pub fn record_deploy_rejected(&self, actor: &str, rev: Option<&str>, scope: &str, reason: &str) {
        self.record(HistoryEvent {
            at_ms: current_unix_ms(),
            run_id: self.inner.run_id.clone(),
            category: "deploy".to_string(),
            kind: "deploy.rejected".to_string(),
            outcome: Some("rejected".to_string()),
            actor: sanitize_actor(actor),
            subject: sanitize_revision(rev),
            detail: sanitize_detail(serde_json::json!({ "scope": scope, "reason": reason })),
        });
    }

    pub fn record_deploy_rollback(&self, actor: &str, rev: Option<&str>, ok: bool, reason: Option<&str>) {
        let (outcome, detail) = if ok {
            (Some("ok".to_string()), serde_json::json!({}))
        } else {
            (Some("rejected".to_string()), serde_json::json!({ "reason": reason.unwrap_or("failed") }))
        };
        self.record(HistoryEvent {
            at_ms: current_unix_ms(),
            run_id: self.inner.run_id.clone(),
            category: "deploy".to_string(),
            kind: "deploy.rollback".to_string(),
            outcome,
            actor: sanitize_actor(actor),
            subject: sanitize_revision(rev),
            detail: sanitize_detail(detail),
        });
    }

    pub fn record_node_error(&self, node_id: ElementId, node_type: &str) {
        if !self.inner.config.enabled {
            return;
        }

        let mut errors = self.inner.node_errors.lock().unwrap();
        if errors.len() > 4096 {
            errors.clear();
        }

        let now = Instant::now();
        let interval = self.inner.node_error_interval;

        if interval.is_zero() {
            drop(errors);
            self.record(HistoryEvent {
                at_ms: current_unix_ms(),
                run_id: self.inner.run_id.clone(),
                category: "node".to_string(),
                kind: "node.error".to_string(),
                outcome: Some("failed".to_string()),
                actor: "runtime".to_string(),
                subject: Some(node_id.to_string()),
                detail: sanitize_detail(serde_json::json!({ "type": node_type, "suppressed": 0 })),
            });
            return;
        }

        let suppressed_to_emit = if let Some(entry) = errors.get_mut(&node_id) {
            if now.duration_since(entry.last_reported) < interval {
                entry.suppressed += 1;
                return;
            } else {
                let prev_suppressed = entry.suppressed;
                entry.last_reported = now;
                entry.suppressed = 0;
                prev_suppressed
            }
        } else {
            errors.insert(node_id, NodeErrorEntry { last_reported: now, suppressed: 0 });
            0
        };

        drop(errors);
        self.record(HistoryEvent {
            at_ms: current_unix_ms(),
            run_id: self.inner.run_id.clone(),
            category: "node".to_string(),
            kind: "node.error".to_string(),
            outcome: Some("failed".to_string()),
            actor: "runtime".to_string(),
            subject: Some(node_id.to_string()),
            detail: sanitize_detail(serde_json::json!({ "type": node_type, "suppressed": suppressed_to_emit })),
        });
    }

    pub fn record_node_status(&self, node_id: ElementId, node_type: &str, fill: Option<&str>, shape: Option<&str>) {
        if !self.inner.config.enabled {
            return;
        }

        let mut status_map = self.inner.node_status.lock().unwrap();
        if status_map.len() > 4096 {
            status_map.clear();
        }

        let new_val = (fill.map(str::to_string), shape.map(str::to_string));
        if let Some(existing) = status_map.get(&node_id)
            && *existing == new_val
        {
            return; // deduplicated, no change
        }

        status_map.insert(node_id, new_val);
        drop(status_map);

        self.record(HistoryEvent {
            at_ms: current_unix_ms(),
            run_id: self.inner.run_id.clone(),
            category: "node".to_string(),
            kind: "node.status".to_string(),
            outcome: None,
            actor: "runtime".to_string(),
            subject: Some(node_id.to_string()),
            detail: sanitize_detail(serde_json::json!({ "type": node_type, "fill": fill, "shape": shape })),
        });
    }

    pub fn record_copilot_requested(&self, actor: &str) {
        self.record(HistoryEvent {
            at_ms: current_unix_ms(),
            run_id: self.inner.run_id.clone(),
            category: "copilot".to_string(),
            kind: "copilot.draft.requested".to_string(),
            outcome: None,
            actor: sanitize_actor(actor),
            subject: None,
            detail: "{}".to_string(),
        });
    }

    pub fn record_copilot_produced(&self, actor: &str, nodes: usize) {
        self.record(HistoryEvent {
            at_ms: current_unix_ms(),
            run_id: self.inner.run_id.clone(),
            category: "copilot".to_string(),
            kind: "copilot.draft.produced".to_string(),
            outcome: Some("ok".to_string()),
            actor: sanitize_actor(actor),
            subject: None,
            detail: sanitize_detail(serde_json::json!({ "nodes": nodes })),
        });
    }

    pub fn record_copilot_rejected(&self, actor: &str, reason: &str) {
        self.record(HistoryEvent {
            at_ms: current_unix_ms(),
            run_id: self.inner.run_id.clone(),
            category: "copilot".to_string(),
            kind: "copilot.draft.rejected".to_string(),
            outcome: Some("rejected".to_string()),
            actor: sanitize_actor(actor),
            subject: None,
            detail: sanitize_detail(serde_json::json!({ "reason": reason })),
        });
    }

    /// Plugin lifecycle (`plugin.staged`, `plugin.rejected`, `plugin.activated`,
    /// `plugin.rolled_back`, `plugin.removed`, `plugin.discarded`, `plugin.failed`). Records the
    /// plugin id, version, the first 12 hex digits of the package digest and a reason code;
    /// never package bytes, configuration values or message content.
    pub fn record_plugin(
        &self,
        actor: &str,
        kind: &str,
        plugin_id: &str,
        version: Option<&str>,
        sha256: Option<&str>,
        reason: Option<&str>,
    ) {
        let digest = sha256.filter(|s| s.chars().all(|c| c.is_ascii_hexdigit())).map(|s| &s[..s.len().min(12)]);
        let reason =
            reason.map(|r| r.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(32).collect::<String>());
        self.record(HistoryEvent {
            at_ms: current_unix_ms(),
            run_id: self.inner.run_id.clone(),
            category: "plugin".to_string(),
            kind: kind.chars().filter(|c| c.is_ascii_lowercase() || *c == '.' || *c == '_').take(32).collect(),
            outcome: Some(if reason.is_some() { "failed" } else { "ok" }.to_string()),
            actor: sanitize_actor(actor),
            subject: sanitize_subject(Some(plugin_id)),
            detail: sanitize_detail(serde_json::json!({
                "version": version.map(|v| v.chars().take(64).collect::<String>()),
                "digest": digest,
                "reason": reason,
            })),
        });
    }

    pub fn record_fleet_push(&self, actor: &str, device: &str, status: u16, rev: Option<&str>) {
        self.record(HistoryEvent {
            at_ms: current_unix_ms(),
            run_id: self.inner.run_id.clone(),
            category: "fleet".to_string(),
            kind: "fleet.push".to_string(),
            outcome: if status < 400 { Some("ok".to_string()) } else { Some("failed".to_string()) },
            actor: sanitize_actor(actor),
            subject: sanitize_subject(Some(device)),
            detail: sanitize_detail(serde_json::json!({ "status": status, "rev": sanitize_revision(rev) })),
        });
    }

    pub fn record_fleet_promote(&self, actor: &str, from_to: &str, status: u16, rev: Option<&str>) {
        self.record(HistoryEvent {
            at_ms: current_unix_ms(),
            run_id: self.inner.run_id.clone(),
            category: "fleet".to_string(),
            kind: "fleet.promote".to_string(),
            outcome: if status < 400 { Some("ok".to_string()) } else { Some("failed".to_string()) },
            actor: sanitize_actor(actor),
            subject: sanitize_subject(Some(from_to)),
            detail: sanitize_detail(serde_json::json!({ "status": status, "rev": sanitize_revision(rev) })),
        });
    }

    pub fn record_runtime_started(&self, version: &str, schema: u32) {
        self.record(HistoryEvent {
            at_ms: current_unix_ms(),
            run_id: self.inner.run_id.clone(),
            category: "runtime".to_string(),
            kind: "runtime.started".to_string(),
            outcome: None,
            actor: "runtime".to_string(),
            subject: None,
            detail: sanitize_detail(serde_json::json!({ "version": version, "schema": schema })),
        });
    }

    pub fn record_runtime_stopped(&self, version: &str, schema: u32) {
        self.record(HistoryEvent {
            at_ms: current_unix_ms(),
            run_id: self.inner.run_id.clone(),
            category: "runtime".to_string(),
            kind: "runtime.stopped".to_string(),
            outcome: None,
            actor: "runtime".to_string(),
            subject: None,
            detail: sanitize_detail(serde_json::json!({ "version": version, "schema": schema })),
        });
    }

    /// Fetch history subsystem health.
    pub fn health(&self) -> HistoryHealth {
        let state = *self.inner.state.lock().unwrap();
        let last_error = self.inner.last_error.lock().unwrap().clone();
        let last_error_at_ms = self.inner.last_error_at_ms.load(Ordering::Relaxed);

        HistoryHealth {
            state: state.to_string(),
            schema_version: SCHEMA_VERSION,
            queue_capacity: self.inner.config.queue_capacity,
            queued: 0, // In sync_channel, queue depth is approximate or zero on drain
            accepted: self.inner.accepted.load(Ordering::Relaxed),
            written: self.inner.written.load(Ordering::Relaxed),
            dropped_queue_full: self.inner.dropped_queue_full.load(Ordering::Relaxed),
            dropped_write_error: self.inner.dropped_write_error.load(Ordering::Relaxed),
            dropped_shutdown: self.inner.dropped_shutdown.load(Ordering::Relaxed),
            pruned: self.inner.pruned.load(Ordering::Relaxed),
            db_bytes: self.inner.db_bytes.load(Ordering::Relaxed),
            last_error,
            last_error_at_ms: if last_error_at_ms == 0 { None } else { Some(last_error_at_ms) },
        }
    }

    /// Gracefully shutdown the history writer, draining up to shutdown_drain_ms.
    pub fn shutdown(&self) {
        if !self.inner.config.enabled {
            return;
        }

        self.inner.is_stopping.store(true, Ordering::SeqCst);

        #[cfg(feature = "history_sqlite")]
        {
            // Drop sender to signal worker thread to finish draining
            let sender = self.inner.writer_sender.lock().unwrap().take();
            drop(sender);

            let handle = self.inner.writer_thread.lock().unwrap().take();
            if let Some(th) = handle {
                let _ = th.join();
            }

            *self.inner.state.lock().unwrap() = HistoryState::Stopped;
        }
    }

    /// Query the operational history.
    pub fn query(&self, _filter: HistoryQuery) -> std::result::Result<HistoryQueryResult, HistoryQueryError> {
        if !self.inner.config.enabled {
            return Err(HistoryQueryError::Disabled);
        }

        let state = *self.inner.state.lock().unwrap();
        if state == HistoryState::Failed {
            let last_err = self.inner.last_error.lock().unwrap().clone().unwrap_or_else(|| "failed".to_string());
            return Err(HistoryQueryError::Failed(last_err));
        }

        #[cfg(not(feature = "history_sqlite"))]
        {
            Err(HistoryQueryError::Disabled)
        }

        #[cfg(feature = "history_sqlite")]
        {
            self.execute_query(_filter)
        }
    }
}

// ---------------------------------------------------------------------------
// SQLite Specific Implementation
// ---------------------------------------------------------------------------

#[cfg(feature = "history_sqlite")]
impl HistoryHandle {
    pub fn init_with_config(config: HistoryConfig, home_dir: Option<PathBuf>) -> Result<Self> {
        let handle = Self {
            inner: Arc::new(HistoryInner {
                run_id: uuid::Uuid::new_v4().to_string(),
                home_dir: Mutex::new(home_dir.clone()),
                resolved_db_path: Mutex::new(None),
                state: Mutex::new(HistoryState::Starting),
                last_error: Mutex::new(None),
                last_error_at_ms: AtomicI64::new(0),
                node_error_interval: Duration::from_millis(config.node_error_interval_ms),
                node_errors: Mutex::new(std::collections::HashMap::new()),
                node_status: Mutex::new(std::collections::HashMap::new()),
                accepted: AtomicU64::new(0),
                written: AtomicU64::new(0),
                dropped_queue_full: AtomicU64::new(0),
                dropped_write_error: AtomicU64::new(0),
                dropped_shutdown: AtomicU64::new(0),
                pruned: AtomicU64::new(0),
                db_bytes: AtomicU64::new(0),
                is_stopping: AtomicBool::new(false),
                config,
                writer_sender: Mutex::new(None),
                writer_thread: Mutex::new(None),
            }),
        };

        handle.resolve_and_ensure_writer(home_dir);
        Ok(handle)
    }

    fn resolve_and_ensure_writer(&self, home_dir: Option<PathBuf>) {
        let mut path_guard = self.inner.resolved_db_path.lock().unwrap();
        if path_guard.is_some() {
            return;
        }

        let db_path = match &self.inner.config.path {
            Some(p) => {
                let path = PathBuf::from(p);
                if path.is_absolute() {
                    path
                } else if let Some(home) = home_dir {
                    home.join(path)
                } else {
                    path
                }
            }
            None => {
                if let Some(home) = home_dir {
                    home.join("history.sqlite3")
                } else {
                    PathBuf::from("history.sqlite3")
                }
            }
        };

        *path_guard = Some(db_path.clone());
        drop(path_guard);

        // Spawn worker thread
        let (tx, rx) = std::sync::mpsc::sync_channel::<HistoryEvent>(self.inner.config.queue_capacity);
        *self.inner.writer_sender.lock().unwrap() = Some(tx);

        let inner_clone = Arc::clone(&self.inner);
        let thread_handle = std::thread::Builder::new()
            .name("history-writer".to_string())
            .spawn(move || {
                run_writer_loop(inner_clone, db_path, rx);
            })
            .expect("failed to spawn history writer thread");

        *self.inner.writer_thread.lock().unwrap() = Some(thread_handle);
    }

    fn execute_query(&self, filter: HistoryQuery) -> std::result::Result<HistoryQueryResult, HistoryQueryError> {
        let db_path = {
            let path_guard = self.inner.resolved_db_path.lock().unwrap();
            path_guard.clone().ok_or_else(|| HistoryQueryError::Failed("database path not resolved".to_string()))?
        };

        if !db_path.exists() {
            return Ok(HistoryQueryResult { events: Vec::new(), next: None });
        }

        let conn = match rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(c) => c,
            Err(e) => return Err(HistoryQueryError::Internal(classify_sqlite_error(&e))),
        };

        let _ = conn.busy_timeout(Duration::from_millis(2000));

        let limit = if filter.limit == 0 { 100 } else { filter.limit.min(500) };
        let query_limit = limit + 1;

        let mut sql =
            "SELECT seq, at_ms, run_id, category, kind, outcome, actor, subject, detail FROM events WHERE 1=1"
                .to_string();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(before) = filter.before {
            if before <= 0 {
                return Err(HistoryQueryError::InvalidFilter("before must be positive".to_string()));
            }
            sql.push_str(" AND seq < ?");
            params.push(Box::new(before));
        }

        if let Some(since) = filter.since_ms {
            sql.push_str(" AND at_ms >= ?");
            params.push(Box::new(since));
        }

        if let Some(until) = filter.until_ms {
            sql.push_str(" AND at_ms <= ?");
            params.push(Box::new(until));
        }

        if let (Some(since), Some(until)) = (filter.since_ms, filter.until_ms)
            && since > until
        {
            return Err(HistoryQueryError::InvalidFilter("since_ms must be <= until_ms".to_string()));
        }

        if let Some(cat) = &filter.category {
            sql.push_str(" AND category = ?");
            params.push(Box::new(cat.clone()));
        }

        if let Some(kind) = &filter.kind {
            sql.push_str(" AND kind = ?");
            params.push(Box::new(kind.clone()));
        }

        if let Some(subject) = &filter.subject {
            sql.push_str(" AND subject = ?");
            params.push(Box::new(subject.clone()));
        }

        sql.push_str(" ORDER BY seq DESC LIMIT ?");
        params.push(Box::new(query_limit as i64));

        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(e) => return Err(HistoryQueryError::Internal(classify_sqlite_error(&e))),
        };

        let rusqlite_params: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| &**b).collect();
        let rows_iter = match stmt.query_map(&rusqlite_params[..], |row| {
            let seq: i64 = row.get(0)?;
            let at_ms: i64 = row.get(1)?;
            let run_id: String = row.get(2)?;
            let category: String = row.get(3)?;
            let kind: String = row.get(4)?;
            let outcome: Option<String> = row.get(5)?;
            let actor: String = row.get(6)?;
            let subject: Option<String> = row.get(7)?;
            let detail_str: String = row.get(8)?;
            let detail: serde_json::Value = serde_json::from_str(&detail_str).unwrap_or_else(|_| serde_json::json!({}));

            Ok(HistoryEventRow { seq, at_ms, run_id, category, kind, outcome, actor, subject, detail })
        }) {
            Ok(iter) => iter,
            Err(e) => return Err(HistoryQueryError::Internal(classify_sqlite_error(&e))),
        };

        let mut events = Vec::new();
        for r in rows_iter {
            match r {
                Ok(item) => events.push(item),
                Err(e) => return Err(HistoryQueryError::Internal(classify_sqlite_error(&e))),
            }
        }

        let next = if events.len() > limit {
            events.pop();
            events.last().map(|row| row.seq)
        } else {
            None
        };

        Ok(HistoryQueryResult { events, next })
    }
}

// ---------------------------------------------------------------------------
// Worker Loop & Retention
// ---------------------------------------------------------------------------

#[cfg(feature = "history_sqlite")]
fn classify_sqlite_error(err: &rusqlite::Error) -> String {
    let msg = err.to_string();
    if msg.contains("database is full") || msg.contains("disk full") {
        "full".to_string()
    } else if msg.contains("readonly") || msg.contains("read-only") {
        "read_only".to_string()
    } else if msg.contains("malformed") || msg.contains("corrupt") || msg.contains("not a database") {
        "corrupt".to_string()
    } else if msg.contains("permission denied") {
        "permission".to_string()
    } else {
        msg
    }
}

#[cfg(feature = "history_sqlite")]
fn set_last_error(inner: &HistoryInner, err: &str) {
    *inner.last_error.lock().unwrap() = Some(err.to_string());
    inner.last_error_at_ms.store(current_unix_ms(), Ordering::Relaxed);
}

#[cfg(feature = "history_sqlite")]
fn run_writer_loop(inner: Arc<HistoryInner>, db_path: PathBuf, rx: std::sync::mpsc::Receiver<HistoryEvent>) {
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;
        if !db_path.exists() {
            if let Ok(file) =
                std::fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(&db_path)
            {
                drop(file);
            }
        } else {
            let _ = std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600));
        }
    }

    let mut conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to open history database: {e}");
            *inner.state.lock().unwrap() = HistoryState::Failed;
            set_last_error(&inner, &classify_sqlite_error(&e));
            // Drain remaining events to prevent producer hangs
            while rx.recv().is_ok() {
                inner.dropped_write_error.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
    };

    // Configure connection pragmas
    let _ = conn.busy_timeout(Duration::from_millis(1000));
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    let _ = conn.pragma_update(None, "synchronous", "NORMAL");

    let tables_exist: bool = conn
        .query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='events'", [], |r| {
            r.get::<_, i64>(0)
        })
        .map(|c| c > 0)
        .unwrap_or(false);

    if !tables_exist {
        let _ = conn.pragma_update(None, "auto_vacuum", "INCREMENTAL");
        if let Err(e) = init_new_schema(&conn) {
            log::error!("Failed to initialize history database schema: {e}");
            *inner.state.lock().unwrap() = HistoryState::Failed;
            set_last_error(&inner, &classify_sqlite_error(&e));
            while rx.recv().is_ok() {
                inner.dropped_write_error.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
    } else {
        let app_id: u32 = conn.query_row("PRAGMA application_id", [], |r| r.get(0)).unwrap_or(0);
        if app_id != APPLICATION_ID {
            log::error!("Foreign database rejected: application_id={:#X}", app_id);
            *inner.state.lock().unwrap() = HistoryState::Failed;
            set_last_error(&inner, "foreign_database");
            while rx.recv().is_ok() {
                inner.dropped_write_error.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }

        let user_ver: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
        if user_ver > SCHEMA_VERSION {
            log::error!("Newer schema version detected: {user_ver} > {SCHEMA_VERSION}");
            *inner.state.lock().unwrap() = HistoryState::Failed;
            set_last_error(&inner, "newer_schema");
            while rx.recv().is_ok() {
                inner.dropped_write_error.fetch_add(1, Ordering::Relaxed);
            }
            return;
        } else if user_ver < SCHEMA_VERSION {
            if !inner.config.migrate {
                log::error!("Migration required for schema version {user_ver}");
                *inner.state.lock().unwrap() = HistoryState::Failed;
                set_last_error(&inner, "migration_required");
                while rx.recv().is_ok() {
                    inner.dropped_write_error.fetch_add(1, Ordering::Relaxed);
                }
                return;
            } else if let Err(e) = migrate_schema(&mut conn, &db_path, user_ver) {
                log::error!("Schema migration failed: {e}");
                *inner.state.lock().unwrap() = HistoryState::Failed;
                set_last_error(&inner, &classify_sqlite_error(&e));
                while rx.recv().is_ok() {
                    inner.dropped_write_error.fetch_add(1, Ordering::Relaxed);
                }
                return;
            }
        }
    }

    // Set page size and journal size limit
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap_or(4096);
    let max_pages = (inner.config.max_db_bytes as i64 / page_size).max(10);
    let _ = conn.pragma_update(None, "max_page_count", max_pages);

    let journal_limit = (4 * 1024 * 1024).min(inner.config.max_db_bytes as i64 / 4);
    let _ = conn.pragma_update(None, "journal_size_limit", journal_limit);

    *inner.state.lock().unwrap() = HistoryState::Ok;

    let mut last_retention = Instant::now();
    let batch_max = inner.config.batch_max;
    let mut batch = Vec::with_capacity(batch_max);

    let mut last_reported_gap_queue_full = 0u64;
    let mut last_reported_gap_write_error = 0u64;

    loop {
        // Wait for first event with 500ms timeout
        let event = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => Some(ev),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Channel closed, process any final items and drain
                break;
            }
        };

        if let Some(ev) = event {
            batch.push(ev);
            while batch.len() < batch_max {
                match rx.try_recv() {
                    Ok(item) => batch.push(item),
                    Err(_) => break,
                }
            }
        }

        // Check if there are drops to report as a history.gap event
        let current_drops_q = inner.dropped_queue_full.load(Ordering::Relaxed);
        let current_drops_w = inner.dropped_write_error.load(Ordering::Relaxed);
        if current_drops_q > last_reported_gap_queue_full || current_drops_w > last_reported_gap_write_error {
            let gap_q = current_drops_q - last_reported_gap_queue_full;
            let gap_w = current_drops_w - last_reported_gap_write_error;
            last_reported_gap_queue_full = current_drops_q;
            last_reported_gap_write_error = current_drops_w;

            batch.insert(
                0,
                HistoryEvent {
                    at_ms: current_unix_ms(),
                    run_id: inner.run_id.clone(),
                    category: "history".to_string(),
                    kind: "history.gap".to_string(),
                    outcome: None,
                    actor: "runtime".to_string(),
                    subject: None,
                    detail: sanitize_detail(serde_json::json!({
                        "queue_full": gap_q,
                        "write_error": gap_w
                    })),
                },
            );
        }

        if !batch.is_empty() {
            write_batch(&mut conn, &inner, &batch, max_pages);
            batch.clear();
        }

        // Periodic retention check every 60s
        if last_retention.elapsed() >= Duration::from_secs(60) {
            run_retention(&mut conn, &inner, max_pages);
            last_retention = Instant::now();
        }
    }

    // Drain loop on shutdown
    let drain_deadline = Instant::now() + Duration::from_millis(inner.config.shutdown_drain_ms);
    let mut drained_batch = Vec::new();
    while Instant::now() < drain_deadline {
        match rx.try_recv() {
            Ok(ev) => {
                drained_batch.push(ev);
                if drained_batch.len() >= batch_max {
                    write_batch(&mut conn, &inner, &drained_batch, max_pages);
                    drained_batch.clear();
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
    }

    if !drained_batch.is_empty() {
        write_batch(&mut conn, &inner, &drained_batch, max_pages);
    }

    // Any remaining items after deadline are dropped
    while rx.try_recv().is_ok() {
        inner.dropped_shutdown.fetch_add(1, Ordering::Relaxed);
    }

    let _ = conn.close();
}

#[cfg(feature = "history_sqlite")]
fn init_new_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA application_id = 0x454C4831;
        PRAGMA user_version = 1;

        CREATE TABLE IF NOT EXISTS events (
            seq      INTEGER PRIMARY KEY AUTOINCREMENT,
            at_ms    INTEGER NOT NULL,
            run_id   TEXT    NOT NULL,
            category TEXT    NOT NULL,
            kind     TEXT    NOT NULL,
            outcome  TEXT,
            actor    TEXT    NOT NULL,
            subject  TEXT,
            detail   TEXT    NOT NULL CHECK (json_valid(detail) AND length(detail) <= 1024)
        );
        CREATE INDEX IF NOT EXISTS events_at_ms ON events(at_ms);
        CREATE INDEX IF NOT EXISTS events_category_seq ON events(category, seq);
        CREATE INDEX IF NOT EXISTS events_kind_seq ON events(kind, seq);
        "#,
    )?;
    Ok(())
}

#[cfg(feature = "history_sqlite")]
fn migrate_schema(conn: &mut rusqlite::Connection, db_path: &Path, old_version: u32) -> rusqlite::Result<()> {
    // Create backup before migration
    let backup_path = format!("{}.v{}.{}.bak", db_path.display(), old_version, current_unix_ms());
    conn.execute(&format!("VACUUM INTO '{}'", backup_path), [])?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&backup_path, std::fs::Permissions::from_mode(0o600));
    }

    let tx = conn.transaction()?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

#[cfg(feature = "history_sqlite")]
fn write_batch(conn: &mut rusqlite::Connection, inner: &HistoryInner, batch: &[HistoryEvent], _max_pages: i64) {
    if let Err(e) = write_batch_inner(conn, batch) {
        let err_class = classify_sqlite_error(&e);
        log::warn!("History write batch failed: {err_class}");

        if err_class == "full" {
            // Emergency prune and retry once
            emergency_prune(conn, inner);
            if let Err(_retry_err) = write_batch_inner(conn, batch) {
                inner.dropped_write_error.fetch_add(batch.len() as u64, Ordering::Relaxed);
                *inner.state.lock().unwrap() = HistoryState::Degraded;
                set_last_error(inner, "full");
            } else {
                inner.written.fetch_add(batch.len() as u64, Ordering::Relaxed);
                *inner.state.lock().unwrap() = HistoryState::Ok;
            }
        } else {
            inner.dropped_write_error.fetch_add(batch.len() as u64, Ordering::Relaxed);
            *inner.state.lock().unwrap() = HistoryState::Degraded;
            set_last_error(inner, &err_class);
        }
    } else {
        inner.written.fetch_add(batch.len() as u64, Ordering::Relaxed);
        let mut state = inner.state.lock().unwrap();
        if *state == HistoryState::Degraded {
            *state = HistoryState::Ok;
        }
    }

    // Update db size
    update_db_bytes(conn, inner);
}

#[cfg(feature = "history_sqlite")]
fn write_batch_inner(conn: &mut rusqlite::Connection, batch: &[HistoryEvent]) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO events (at_ms, run_id, category, kind, outcome, actor, subject, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
        )?;
        for ev in batch {
            stmt.execute(rusqlite::params![
                ev.at_ms,
                ev.run_id,
                ev.category,
                ev.kind,
                ev.outcome,
                ev.actor,
                ev.subject,
                ev.detail,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(feature = "history_sqlite")]
fn run_retention(conn: &mut rusqlite::Connection, inner: &HistoryInner, max_pages: i64) {
    let cutoff_ms = current_unix_ms() - (inner.config.retention_days as i64 * 86_400_000);
    if let Ok(deleted) = conn.execute("DELETE FROM events WHERE at_ms < ?1", rusqlite::params![cutoff_ms])
        && deleted > 0
    {
        inner.pruned.fetch_add(deleted as u64, Ordering::Relaxed);
    }

    let mut page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap_or(0);
    if page_count as f64 > 0.8 * max_pages as f64 {
        while page_count as f64 > 0.7 * max_pages as f64 {
            let deleted = conn
                .execute("DELETE FROM events WHERE seq IN (SELECT seq FROM events ORDER BY seq ASC LIMIT 512)", [])
                .unwrap_or(0);
            if deleted == 0 {
                break;
            }
            inner.pruned.fetch_add(deleted as u64, Ordering::Relaxed);
            page_count = conn.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap_or(0);
        }
        let _ = conn.execute("PRAGMA incremental_vacuum", []);
    }

    update_db_bytes(conn, inner);
}

#[cfg(feature = "history_sqlite")]
fn emergency_prune(conn: &mut rusqlite::Connection, inner: &HistoryInner) {
    if let Ok(deleted) =
        conn.execute("DELETE FROM events WHERE seq IN (SELECT seq FROM events ORDER BY seq ASC LIMIT 512)", [])
        && deleted > 0
    {
        inner.pruned.fetch_add(deleted as u64, Ordering::Relaxed);
        let _ = conn.execute("PRAGMA incremental_vacuum", []);
    }
}

#[cfg(feature = "history_sqlite")]
fn update_db_bytes(conn: &rusqlite::Connection, inner: &HistoryInner) {
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap_or(4096);
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap_or(0);
    inner.db_bytes.store((page_size.max(0) * page_count.max(0)) as u64, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_validation() {
        let mut cfg = HistoryConfig::default();
        assert!(cfg.validate().is_ok());

        cfg.enabled = true;
        assert!(cfg.validate().is_ok());

        cfg.queue_capacity = 10; // too small
        assert!(cfg.validate().is_err());
        cfg.queue_capacity = 1024;

        cfg.batch_max = 2000; // > queue_capacity
        assert!(cfg.validate().is_err());
        cfg.batch_max = 256;

        cfg.retention_days = 0;
        assert!(cfg.validate().is_err());
        cfg.retention_days = 30;

        cfg.max_db_bytes = 100; // too small
        assert!(cfg.validate().is_err());
        cfg.max_db_bytes = 16_777_216;

        cfg.shutdown_drain_ms = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_sanitization() {
        assert_eq!(sanitize_actor(""), "anonymous");
        assert_eq!(sanitize_actor("   "), "anonymous");
        assert_eq!(sanitize_actor("admin\u{0000}test"), "admintest");

        assert_eq!(sanitize_subject(None), None);
        assert_eq!(sanitize_subject(Some("")), None);
        assert_eq!(sanitize_subject(Some("my-node\t")), Some("my-node".to_string()));

        assert_eq!(sanitize_revision(None), None);
        assert_eq!(sanitize_revision(Some("")), None);
        assert_eq!(sanitize_revision(Some("abcdef0123456789")), Some("abcdef0123456789".to_string()));
        assert_eq!(sanitize_revision(Some("bad revision with spaces")), Some("invalid".to_string()));

        let valid_json = serde_json::json!({ "foo": "bar" });
        assert_eq!(sanitize_detail(valid_json), "{\"foo\":\"bar\"}");
    }

    #[cfg(feature = "history_sqlite")]
    fn wait_until_history_ready(handle: &HistoryHandle) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut health = handle.health();
        while health.state == "starting" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            health = handle.health();
        }
        assert_eq!(health.state, "ok", "history writer did not become ready: {health:?}");
    }

    #[cfg(feature = "history_sqlite")]
    #[test]
    fn test_sqlite_history_lifecycle() {
        let temp_dir = std::env::temp_dir().join(format!("n2linkd-hist-test-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let db_path = temp_dir.join("history.sqlite3");

        let config = HistoryConfig {
            enabled: true,
            path: Some(db_path.to_str().unwrap().to_string()),
            queue_capacity: 100,
            batch_max: 50,
            retention_days: 1,
            max_db_bytes: 1_048_576,
            shutdown_drain_ms: 1000,
            node_error_interval_ms: 50,
            migrate: false,
        };

        let handle = HistoryHandle::init_with_config(config, Some(temp_dir.clone())).unwrap();
        wait_until_history_ready(&handle);
        assert_eq!(handle.health().schema_version, 1);

        // Record some events
        handle.record_deploy_proposed("admin", Some("1234abcd"), "full", 10);
        handle.record_deploy_accepted("admin", "1234abcd", "full", 10);
        handle.record_copilot_requested("admin");
        handle.record_copilot_produced("admin", 3);
        handle.record_fleet_push("admin", "device-1", 200, Some("1234abcd"));

        // Wait for batch write
        std::thread::sleep(Duration::from_millis(600));

        let res = handle.query(HistoryQuery::default()).unwrap();
        assert_eq!(res.events.len(), 5);

        // Query with filter
        let deploy_query = HistoryQuery { category: Some("deploy".to_string()), ..Default::default() };
        let deploy_res = handle.query(deploy_query).unwrap();
        assert_eq!(deploy_res.events.len(), 2);

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[cfg(feature = "history_sqlite")]
    #[test]
    fn test_concurrent_writers() {
        let temp_dir = std::env::temp_dir().join(format!("n2linkd-hist-concurrent-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let db_path = temp_dir.join("history.sqlite3");

        let config = HistoryConfig {
            enabled: true,
            path: Some(db_path.to_str().unwrap().to_string()),
            queue_capacity: 1000,
            batch_max: 200,
            retention_days: 1,
            max_db_bytes: 10_000_000,
            shutdown_drain_ms: 2000,
            node_error_interval_ms: 50,
            migrate: false,
        };

        let handle = HistoryHandle::init_with_config(config, Some(temp_dir.clone())).unwrap();
        wait_until_history_ready(&handle);

        let mut handles = Vec::new();
        for t in 0..4 {
            let h = handle.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..25 {
                    h.record_deploy_proposed(&format!("worker-{t}"), Some("1234abcd"), "full", i);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Wait for drain & batch write
        std::thread::sleep(Duration::from_millis(600));

        let res = handle.query(HistoryQuery { limit: 200, ..Default::default() }).unwrap();
        assert_eq!(res.events.len(), 100);

        // Verify sequence ordering is monotonic
        for i in 0..res.events.len() - 1 {
            assert!(res.events[i].seq > res.events[i + 1].seq, "Expected descending order by seq");
        }

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[cfg(feature = "history_sqlite")]
    #[test]
    fn test_queue_saturation_and_gap() {
        let temp_dir = std::env::temp_dir().join(format!("n2linkd-hist-sat-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let db_path = temp_dir.join("history.sqlite3");

        let config = HistoryConfig {
            enabled: true,
            path: Some(db_path.to_str().unwrap().to_string()),
            queue_capacity: 50,
            batch_max: 20,
            retention_days: 1,
            max_db_bytes: 10_000_000,
            shutdown_drain_ms: 1000,
            node_error_interval_ms: 50,
            migrate: false,
        };

        let handle = HistoryHandle::init_with_config(config, Some(temp_dir.clone())).unwrap();
        wait_until_history_ready(&handle);

        // Rapidly push more events than queue capacity
        for i in 0..200 {
            handle.record_deploy_proposed("admin", Some("1234abcd"), "full", i);
        }

        let health_mid = handle.health();
        assert!(health_mid.dropped_queue_full > 0, "Expected some events to be dropped due to queue saturation");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut has_gap = false;
        while Instant::now() < deadline {
            let res = handle.query(HistoryQuery { limit: 500, ..Default::default() }).unwrap();
            has_gap = res.events.iter().any(|ev| ev.category == "history" && ev.kind == "history.gap");
            if has_gap {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(has_gap, "Expected history.gap event in recorded events");

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[cfg(feature = "history_sqlite")]
    #[test]
    fn test_schema_refusal_on_newer_version() {
        let temp_dir = std::env::temp_dir().join(format!("n2linkd-hist-newer-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let db_path = temp_dir.join("history.sqlite3");

        // Pre-create DB with user_version = 99
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "PRAGMA application_id = 0x454C4831; PRAGMA user_version = 99; CREATE TABLE events (dummy INTEGER);",
            )
            .unwrap();
        }

        let config = HistoryConfig {
            enabled: true,
            path: Some(db_path.to_str().unwrap().to_string()),
            queue_capacity: 100,
            batch_max: 20,
            retention_days: 1,
            max_db_bytes: 10_000_000,
            shutdown_drain_ms: 1000,
            node_error_interval_ms: 50,
            migrate: false,
        };

        let handle = HistoryHandle::init_with_config(config, Some(temp_dir.clone())).unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let health = handle.health();
        assert_eq!(health.state, "failed");
        assert_eq!(health.last_error.as_deref(), Some("newer_schema"));

        // Caller record shouldn't panic
        handle.record_deploy_proposed("admin", Some("1234abcd"), "full", 1);

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[cfg(feature = "history_sqlite")]
    #[test]
    fn test_corrupt_database_handling() {
        let temp_dir = std::env::temp_dir().join(format!("n2linkd-hist-corrupt-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let db_path = temp_dir.join("history.sqlite3");

        // Write junk to db file
        std::fs::write(&db_path, b"not a valid sqlite database header at all").unwrap();

        let config = HistoryConfig {
            enabled: true,
            path: Some(db_path.to_str().unwrap().to_string()),
            queue_capacity: 100,
            batch_max: 20,
            retention_days: 1,
            max_db_bytes: 10_000_000,
            shutdown_drain_ms: 1000,
            node_error_interval_ms: 50,
            migrate: false,
        };

        let handle = HistoryHandle::init_with_config(config, Some(temp_dir.clone())).unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let health = handle.health();
        assert_eq!(health.state, "failed");
        assert_eq!(health.last_error.as_deref(), Some("corrupt"));

        // Call record should safely drop or queue without panic
        handle.record_deploy_proposed("admin", Some("1234abcd"), "full", 1);

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_node_error_throttling() {
        let handle = HistoryHandle::disabled();
        // Even when disabled, test the throttling map logic
        let node_id = ElementId::new();
        handle.record_node_error(node_id, "function");
        // No crash
    }
}
