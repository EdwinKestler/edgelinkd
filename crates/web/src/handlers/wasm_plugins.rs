use axum::Json;
use axum::http::StatusCode;
use serde_json::{Value, json};

/// The plugin store (stage/quarantine/activate/rollback) is not implemented in this prototype.
/// Answer that plainly instead of returning an empty list that looks like real state.
pub async fn list_plugins() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "code": "not_supported",
            "message": "the WASM plugin store is not implemented in this prototype"
        })),
    )
}
