//! Append-only record of who changed the device. One JSON object per line.
//!
//! The file is `<flows directory>/audit.log`. It records deploys, logins, library saves, and
//! fleet pushes. It is not a message historian.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Extension;
use axum::Json;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, RwLock};

use super::WebState;
use super::reply::api_error;

pub struct AuditLog {
    home: RwLock<Option<PathBuf>>,
    write: Mutex<()>,
}

impl AuditLog {
    pub fn new() -> Self {
        Self { home: RwLock::new(None), write: Mutex::new(()) }
    }

    pub async fn set_home(&self, home: PathBuf) {
        *self.home.write().await = Some(home);
    }

    pub async fn record(&self, actor: &str, event: &str, rev: Option<&str>) -> Result<(), String> {
        let _write = self.write.lock().await;
        let home = self.home.read().await.clone().ok_or_else(|| "audit log is not configured".to_string())?;
        let path = home.join("audit.log");
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|err| err.to_string())?;
        }
        let line = json!({
            "at": unix_ms(),
            "actor": actor,
            "event": event,
            "rev": rev,
        });
        let mut text = serde_json::to_string(&line).map_err(|err| err.to_string())?;
        text.push('\n');
        let mut file =
            tokio::fs::OpenOptions::new().create(true).append(true).open(&path).await.map_err(|err| err.to_string())?;
        file.write_all(text.as_bytes()).await.map_err(|err| err.to_string())?;
        file.flush().await.map_err(|err| err.to_string())?;
        Ok(())
    }

    pub async fn recent(&self, limit: usize) -> Result<Vec<Value>, String> {
        let home = self.home.read().await.clone().ok_or_else(|| "audit log is not configured".to_string())?;
        let path = home.join("audit.log");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text = tokio::fs::read_to_string(&path).await.map_err(|err| err.to_string())?;
        let mut rows = Vec::new();
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                rows.push(value);
            }
        }
        let start = rows.len().saturating_sub(limit);
        Ok(rows.split_off(start))
    }
}

impl Default for AuditLog {
    fn default() -> Self {
        Self::new()
    }
}

fn unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_millis() as i64).unwrap_or(0)
}

pub async fn get_audit(Extension(state): Extension<std::sync::Arc<WebState>>, _headers: HeaderMap) -> Response {
    match state.audit.recent(200).await {
        Ok(rows) => Json(rows).into_response(),
        Err(err) => api_error(axum::http::StatusCode::NOT_FOUND, "not_found", &err),
    }
}
