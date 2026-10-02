//! Node-RED library API handlers
//!
//! - GET /library/:type         => get_library_entries
//! - GET /library/:type/*name   => get_library_entry
//! - POST /library/:type/*name  => post_library_entry
//!
//! Library files are stored under static_dir/nodes/examples/ for type=examples, etc.

use crate::handlers::WebState;
use crate::handlers::reply::api_error;
use axum::{
    Extension, Json,
    extract::Path,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use serde_json::{Map, Value, json};
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncWriteExt;

/// List all entries under /library/:type (Node-RED compatible for examples/flows)
pub async fn get_library_entries(
    Extension(state): Extension<Arc<WebState>>,
    Path(lib_type): Path<String>,
) -> impl IntoResponse {
    // Node-RED expects /library/examples/flows/ to return all package names (nodes/*/examples/flows/ 存在的包)
    if lib_type == "examples/flows" {
        let mut pkgs = Vec::new();
        let nodes_dir = state.static_dir.join("nodes");
        if let Ok(mut nodes) = fs::read_dir(&nodes_dir).await {
            while let Ok(Some(entry)) = nodes.next_entry().await {
                let pkg_name = entry.file_name().to_string_lossy().to_string();
                let flows_dir = entry.path().join("examples").join("flows");
                if fs::metadata(&flows_dir).await.map(|m| m.is_dir()).unwrap_or(false) {
                    pkgs.push(pkg_name);
                }
            }
        }
        return Json(pkgs).into_response();
    }

    // Node-RED expects /library/examples/flows/{pkg}/ to return分组/文件名
    if let Some(("examples/flows", pkg)) = lib_type.rsplit_once('/') {
        let flows_dir = state.static_dir.join("nodes").join(pkg).join("examples").join("flows");
        let mut groups = Vec::new();
        if let Ok(mut dir) = fs::read_dir(&flows_dir).await {
            while let Ok(Some(entry)) = dir.next_entry().await {
                let name = entry.file_name().to_string_lossy().to_string();
                groups.push(name);
            }
        }
        return Json(groups).into_response();
    }

    // 默认兼容原有逻辑
    let base_dir = state.static_dir.join("nodes").join(&lib_type);
    let mut entries = Vec::new();
    if let Ok(mut dir) = fs::read_dir(&base_dir).await {
        while let Ok(Some(entry)) = dir.next_entry().await {
            if let Ok(file_type) = entry.file_type().await {
                let name = entry.file_name().to_string_lossy().to_string();
                if file_type.is_file() {
                    entries.push(name);
                } else if file_type.is_dir() {
                    entries.push(format!("{}/", name));
                }
            }
        }
    }
    Json(json!({ "files": entries })).into_response()
}

/// Get a specific entry under /library/:type/*name
pub async fn get_library_entry(
    Extension(state): Extension<Arc<WebState>>,
    Path((lib_type, name)): Path<(String, String)>,
) -> impl IntoResponse {
    if lib_type == "local" {
        return local_get(&state, &name).await;
    }
    if lib_type == "examples" {
        return examples_get(&state, &name).await;
    }
    let file_path = state.static_dir.join("nodes").join(&lib_type).join(&name);
    if !file_path.starts_with(&state.static_dir) {
        return (StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    match fs::read_to_string(&file_path).await {
        Ok(content) => {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
            (headers, content).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "File not found").into_response(),
    }
}

/// Save/update a library entry under /library/:type/*name
pub async fn post_library_entry(
    Extension(state): Extension<Arc<WebState>>,
    headers: HeaderMap,
    Path((lib_type, name)): Path<(String, String)>,
    body: String,
) -> impl IntoResponse {
    if lib_type == "examples" {
        return api_error(StatusCode::FORBIDDEN, "not_supported", "examples library is read-only");
    }
    if lib_type == "local" {
        return local_post(&state, &headers, &name, &body).await;
    }
    let file_path = state.static_dir.join("nodes").join(&lib_type).join(&name);
    if !file_path.starts_with(&state.static_dir) {
        return (StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    if let Some(parent) = file_path.parent()
        && fs::create_dir_all(parent).await.is_err()
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to create directory").into_response();
    }
    match fs::File::create(&file_path).await {
        Ok(mut file) => {
            if file.write_all(body.as_bytes()).await.is_ok() {
                (StatusCode::OK, "Saved").into_response()
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, "Write failed").into_response()
            }
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "File create failed").into_response(),
    }
}

/// `GET /library/examples/flows/` lists packages. A deeper path lists that package's examples.
async fn examples_get(state: &WebState, name: &str) -> axum::response::Response {
    let name = name.trim_matches('/');
    let Some(rest) = name.strip_prefix("flows") else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "library entry was not found");
    };
    let rest = rest.trim_matches('/');
    if rest.is_empty() {
        return Json(example_packages(&state.static_dir.join("nodes")).await).into_response();
    }
    let Some((pkg, sub)) = split_example_path(rest) else {
        return api_error(StatusCode::FORBIDDEN, "forbidden", "library path is not allowed");
    };
    let mut flows_dir = state.static_dir.join("nodes");
    for part in pkg.split('/') {
        flows_dir.push(part);
    }
    flows_dir.push("examples");
    flows_dir.push("flows");
    if !flows_dir.starts_with(&state.static_dir) {
        return api_error(StatusCode::FORBIDDEN, "forbidden", "library path is not allowed");
    }
    let target = if sub.is_empty() {
        flows_dir.clone()
    } else {
        match safe_join(&flows_dir, &sub) {
            Ok(path) => path,
            Err(()) => return api_error(StatusCode::FORBIDDEN, "forbidden", "library path is not allowed"),
        }
    };
    if !target.starts_with(&flows_dir) {
        return api_error(StatusCode::FORBIDDEN, "forbidden", "library path is not allowed");
    }
    let Ok(meta) = fs::metadata(&target).await else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "library entry was not found");
    };
    if meta.is_dir() {
        return Json(list_dir(&target).await).into_response();
    }
    file_body("flows", &target).await
}

fn split_example_path(rest: &str) -> Option<(String, String)> {
    let mut parts = rest.split('/').filter(|part| !part.is_empty());
    let first = parts.next()?;
    if first == ".." || first.contains('\\') {
        return None;
    }
    let pkg = if let Some(scope) = first.strip_prefix('@') {
        if scope.is_empty() {
            return None;
        }
        let second = parts.next()?;
        if second == ".." || second.contains('\\') || second.is_empty() {
            return None;
        }
        format!("{first}/{second}")
    } else {
        first.to_string()
    };
    let sub_parts: Vec<&str> = parts.collect();
    if sub_parts.iter().any(|part| *part == ".." || part.contains('\\')) {
        return None;
    }
    Some((pkg, sub_parts.join("/")))
}

async fn example_packages(nodes_dir: &StdPath) -> Vec<String> {
    let mut pkgs = Vec::new();
    let Ok(mut nodes) = fs::read_dir(nodes_dir).await else {
        return pkgs;
    };
    let mut entries = Vec::new();
    while let Ok(Some(entry)) = nodes.next_entry().await {
        entries.push(entry);
    }
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if name.starts_with('@') {
            if let Ok(mut inner) = fs::read_dir(entry.path()).await {
                while let Ok(Some(child)) = inner.next_entry().await {
                    let child_name = child.file_name().to_string_lossy().to_string();
                    let flows = child.path().join("examples").join("flows");
                    if fs::metadata(&flows).await.map(|item| item.is_dir()).unwrap_or(false) {
                        pkgs.push(format!("{name}/{child_name}"));
                    }
                }
            }
            continue;
        }
        let flows = entry.path().join("examples").join("flows");
        if fs::metadata(&flows).await.map(|item| item.is_dir()).unwrap_or(false) {
            pkgs.push(name);
        }
    }
    pkgs.sort();
    pkgs
}

async fn library_home(state: &WebState) -> Option<PathBuf> {
    let guard = state.flows_file_path.read().await;
    guard
        .as_ref()
        .and_then(|path| path.parent())
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| parent.join("lib"))
}

fn safe_join(root: &StdPath, rel: &str) -> Result<PathBuf, ()> {
    let mut out = root.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains('\\') {
            return Err(());
        }
        out.push(part);
    }
    Ok(out)
}

fn split_local(name: &str) -> Option<(String, String)> {
    let name = name.trim_matches('/');
    if name.is_empty() {
        return None;
    }
    match name.split_once('/') {
        Some((kind, path)) => Some((kind.to_string(), path.trim_matches('/').to_string())),
        None => Some((name.to_string(), String::new())),
    }
}

async fn local_get(state: &WebState, name: &str) -> axum::response::Response {
    let Some(root) = library_home(state).await else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "library home is not configured");
    };
    let Some((kind, path)) = split_local(name) else {
        return Json(Vec::<Value>::new()).into_response();
    };
    let Ok(target) = safe_join(&root.join(&kind), &path) else {
        return api_error(StatusCode::FORBIDDEN, "forbidden", "library path is not allowed");
    };
    if !target.starts_with(&root) {
        return api_error(StatusCode::FORBIDDEN, "forbidden", "library path is not allowed");
    }
    if let Ok(meta) = fs::metadata(&target).await {
        if meta.is_dir() {
            return Json(list_dir(&target).await).into_response();
        }
        if meta.is_file() {
            return file_body(&kind, &target).await;
        }
    }
    if kind == "flows" && !path.is_empty() && !path.ends_with(".json") {
        let alt =
            target.with_file_name(format!("{}.json", target.file_name().and_then(|n| n.to_str()).unwrap_or_default()));
        if fs::metadata(&alt).await.map(|item| item.is_file()).unwrap_or(false) {
            return file_body(&kind, &alt).await;
        }
    }
    if path.is_empty() || name.ends_with('/') {
        return Json(Vec::<Value>::new()).into_response();
    }
    api_error(StatusCode::NOT_FOUND, "not_found", "library entry was not found")
}

async fn list_dir(dir: &StdPath) -> Vec<Value> {
    let mut names = Vec::new();
    if let Ok(mut read) = fs::read_dir(dir).await {
        while let Ok(Some(entry)) = read.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            names.push((name, entry.path()));
        }
    }
    names.sort_by(|left, right| left.0.cmp(&right.0));
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for (name, path) in names {
        if fs::metadata(&path).await.map(|item| item.is_dir()).unwrap_or(false) {
            dirs.push(Value::String(name));
        } else {
            let mut meta = file_meta(&path).await;
            meta.insert("fn".to_string(), Value::String(name));
            files.push(Value::Object(meta));
        }
    }
    dirs.extend(files);
    dirs
}

async fn file_meta(path: &StdPath) -> Map<String, Value> {
    let mut meta = Map::new();
    let Ok(text) = fs::read_to_string(path).await else {
        return meta;
    };
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("// ") else {
            break;
        };
        let Some((key, value)) = rest.split_once(": ") else {
            break;
        };
        if key.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
            meta.insert(key.to_string(), Value::String(from_single_line(value)));
        } else {
            break;
        }
    }
    meta
}

async fn file_body(kind: &str, path: &StdPath) -> axum::response::Response {
    let Ok(text) = fs::read_to_string(path).await else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "library entry was not found");
    };
    let mut body = String::new();
    let mut scanning = true;
    for line in text.lines() {
        if scanning && is_meta_line(line) {
            continue;
        }
        scanning = false;
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(line);
    }
    let mut headers = HeaderMap::new();
    let content_type = if kind == "flows" { "application/json" } else { "text/plain" };
    headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    (headers, body).into_response()
}

fn is_meta_line(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("// ") else {
        return false;
    };
    let Some((key, _)) = rest.split_once(": ") else {
        return false;
    };
    key.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn from_single_line(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn to_single_line(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\n', "\\n")
}

async fn local_post(state: &WebState, headers: &HeaderMap, name: &str, body: &str) -> axum::response::Response {
    let Some(root) = library_home(state).await else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "library home is not configured");
    };
    let Some((kind, mut path)) = split_local(name) else {
        return api_error(StatusCode::BAD_REQUEST, "bad_request", "library entry has no name");
    };
    if path.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "bad_request", "library entry has no name");
    }
    let parsed: Value = match serde_json::from_str(body) {
        Ok(parsed) => parsed,
        Err(_) => return api_error(StatusCode::BAD_REQUEST, "bad_request", "library body is not json"),
    };
    let (meta, stored) = if kind == "flows" {
        if !path.ends_with(".json") {
            path.push_str(".json");
        }
        let pretty = serde_json::to_string_pretty(&parsed).unwrap_or_else(|_| body.to_string());
        (Map::new(), pretty)
    } else {
        let Some(object) = parsed.as_object() else {
            return api_error(StatusCode::BAD_REQUEST, "bad_request", "library body is not an object");
        };
        let Some(text) = object.get("text").and_then(Value::as_str) else {
            return api_error(StatusCode::BAD_REQUEST, "bad_request", "library body has no text");
        };
        let mut meta = Map::new();
        for (key, value) in object {
            if key == "text" {
                continue;
            }
            if let Some(text) = value.as_str() {
                meta.insert(key.clone(), Value::String(text.to_string()));
            }
        }
        (meta, text.to_string())
    };
    let Ok(target) = safe_join(&root.join(&kind), &path) else {
        return api_error(StatusCode::FORBIDDEN, "forbidden", "library path is not allowed");
    };
    if !target.starts_with(root.join(&kind)) {
        return api_error(StatusCode::FORBIDDEN, "forbidden", "library path is not allowed");
    }
    if let Some(parent) = target.parent()
        && fs::create_dir_all(parent).await.is_err()
    {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, "unexpected_error", "library directory was not created");
    }
    let mut header = String::new();
    for (key, value) in &meta {
        if let Some(text) = value.as_str() {
            header.push_str(&format!("// {key}: {}\n", to_single_line(text)));
        }
    }
    if fs::write(&target, format!("{header}{stored}")).await.is_err() {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, "unexpected_error", "library entry was not written");
    }
    let actor = state.auth.actor_from_headers(headers);
    let _ = state.audit.record(&actor.username, "library.save", None).await;
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::create_all_routes;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn router(with_home: bool) -> (axum::Router, TempDir) {
        let dir = TempDir(std::env::temp_dir().join(format!("edgelinkd-lib-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir_all(&dir.0).unwrap();
        let state = WebState::new();
        if with_home {
            let flows = dir.0.join("flows.json");
            std::fs::write(&flows, b"[]").unwrap();
            state.set_flows_file_path(flows).await;
        }
        (create_all_routes(&state).layer(Extension(state)), dir)
    }

    async fn call(router: &axum::Router, method: &str, uri: &str, body: Option<&str>) -> (StatusCode, String) {
        let mut builder = Request::builder().method(method).uri(uri);
        let payload = if let Some(body) = body {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            body.as_bytes().to_vec()
        } else {
            Vec::new()
        };
        let response = router.clone().oneshot(builder.body(Body::from(payload)).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[test]
    fn a_parent_segment_is_rejected() {
        let root = StdPath::new("/tmp/lib/flows");
        assert!(safe_join(root, "../secret").is_err());
        assert!(safe_join(root, r"a\b").is_err());
        assert!(safe_join(root, "pump").unwrap().starts_with(root));
    }

    #[tokio::test]
    async fn local_flows_and_functions_round_trip() {
        let (router, dir) = router(true).await;
        let (status, _) =
            call(&router, "POST", "/library/local/flows/pump", Some(r#"{"id":"pump","type":"subflow"}"#)).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) =
            call(&router, "POST", "/library/local/functions/check", Some(r#"{"name":"Check","text":"return msg;"}"#))
                .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, body) = call(&router, "GET", "/library/local/flows", None).await;
        assert_eq!(status, StatusCode::OK);
        let listed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(listed[0]["fn"], "pump.json");

        let (status, body) = call(&router, "GET", "/library/local/flows/pump", None).await;
        assert_eq!(status, StatusCode::OK);
        let stored: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(stored["type"], "subflow");

        let (status, body) = call(&router, "GET", "/library/local/functions", None).await;
        assert_eq!(status, StatusCode::OK);
        let listed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(listed[0]["fn"], "check");
        assert_eq!(listed[0]["name"], "Check");
        let (status, body) = call(&router, "GET", "/library/local/functions/check", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "return msg;");

        let (status, body) = call(&router, "POST", "/library/local/flows/../secret", Some("{}")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("not allowed"));

        let log = std::fs::read_to_string(dir.0.join("audit.log")).unwrap();
        assert!(log.contains("library.save"));
        assert!(log.contains("anonymous"));
        assert!(log.contains("\"rev\":null"));
    }

    #[tokio::test]
    async fn examples_flows_lists_packages_that_ship_examples() {
        let dir = TempDir(std::env::temp_dir().join(format!("edgelinkd-examples-{}", uuid::Uuid::new_v4())));
        let sample = dir.0.join("nodes/core/examples/flows");
        std::fs::create_dir_all(&sample).unwrap();
        std::fs::write(sample.join("sample.json"), b"[]").unwrap();
        let state = WebState::assemble(
            Arc::new(crate::models::RedSystemSettings::default()),
            dir.0.clone(),
            None,
            crate::handlers::auth::AdminAuth::open(),
            crate::handlers::fleet::Fleet::disabled(),
        );
        let router = create_all_routes(&state).layer(Extension(state));
        let (status, body) = call(&router, "GET", "/library/examples/flows/", None).await;
        assert_eq!(status, StatusCode::OK);
        let listed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(listed, json!(["core"]));
        let (status, body) = call(&router, "GET", "/library/examples/flows", None).await;
        assert_eq!(status, StatusCode::OK);
        let listed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(listed, json!(["core"]));
        let (status, body) = call(&router, "GET", "/library/examples/flows/core", None).await;
        assert_eq!(status, StatusCode::OK);
        let listed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(listed[0]["fn"], "sample.json");
        let (status, body) = call(&router, "POST", "/library/examples/flows/core/sample", Some("{}")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("read-only"));
    }

    #[tokio::test]
    async fn library_without_a_home_is_not_found() {
        let (router, _dir) = router(false).await;
        let (status, body) = call(&router, "GET", "/library/local/flows", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("library home is not configured"));
    }
}
