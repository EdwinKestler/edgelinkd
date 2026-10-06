//! AI nodes. Submodules are feature-gated so `nodes_ai_text` does not pull `reqwest`.

#[cfg(feature = "nodes_ai")]
mod adapter;
#[cfg(feature = "nodes_ai")]
mod chat;
#[cfg(feature = "nodes_ai")]
mod provider;

#[cfg(feature = "nodes_ai_text")]
mod schema;
#[cfg(feature = "nodes_ai_text")]
mod split;
#[cfg(feature = "nodes_ai_text")]
mod structured;

#[cfg(feature = "nodes_ai_embeddings")]
mod embed;

#[cfg(feature = "nodes_ai_agent")]
mod agent;
#[cfg(feature = "nodes_ai_agent")]
mod tools;

#[cfg(feature = "nodes_ai")]
use std::time::Duration;

#[cfg(feature = "nodes_ai")]
use crate::N2linkError;
#[cfg(feature = "nodes_ai")]
use crate::runtime::engine::Engine;
#[cfg(feature = "nodes_ai")]
use crate::runtime::model::ElementId;

#[cfg(feature = "nodes_ai")]
use self::adapter::{ChatMessage, ChatRequest, complete_with_policy};
#[cfg(feature = "nodes_ai")]
use self::provider::AiProviderNode;

/// Complete one server-side prompt with a deployed `ai-provider` configuration.
///
/// The caller receives model text only. Provider settings and credentials never cross the crate
/// boundary and must not be logged by callers.
#[cfg(feature = "nodes_ai")]
pub(crate) async fn complete_for_engine(
    engine: &Engine,
    provider_id: &str,
    model: Option<&str>,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    timeout: Duration,
) -> crate::Result<String> {
    let id: ElementId =
        provider_id.parse().map_err(|_| N2linkError::invalid_operation("AI provider id is not a node id"))?;
    let global = engine
        .find_global_node_by_id(&id)
        .ok_or_else(|| N2linkError::invalid_operation("AI provider is not deployed"))?;
    let provider = global
        .as_any()
        .downcast_ref::<AiProviderNode>()
        .ok_or_else(|| N2linkError::invalid_operation("selected node is not an ai-provider"))?;
    let model = model.filter(|value| !value.trim().is_empty()).unwrap_or(&provider.default_model);
    let request = ChatRequest {
        model: model.to_owned(),
        messages: vec![ChatMessage { role: "user".to_owned(), content: prompt.to_owned() }],
        system: Some(system.to_owned()),
        temperature: None,
        max_tokens: Some(max_tokens),
        timeout,
    };
    let egress = provider.egress.snapshot();
    let response = complete_with_policy(&provider.client, &egress, &provider.settings, &request).await?;
    Ok(response.text)
}

#[cfg(all(test, feature = "nodes_ai"))]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{Value, json};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn server_side_completion_omits_temperature() {
        let app = Router::new().route(
            "/v1/responses",
            post(|Json(body): Json<Value>| async move {
                assert_eq!(body["model"], "reasoning-model");
                assert_eq!(body["max_output_tokens"], 256);
                assert!(body.get("temperature").is_none());
                Json(json!({ "output_text": "draft", "model": "reasoning-model" }))
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let flows = json!([{
            "id": "0000000000000001",
            "type": "ai-provider",
            "provider": "openai",
            "baseUrl": format!("http://{address}/v1"),
            "defaultModel": "reasoning-model",
            "credentials": { "apiKey": "secret-key" }
        }]);
        let cfg = config::Config::builder()
            .add_source(config::File::from_str(
                &format!(
                    r#"
                    [runtime.context]
                    default = "memory"

                    [runtime.context.stores]
                    memory = {{ provider = "memory" }}

                    [egress]
                    mode = "enforce"

                    [[egress.allow]]
                    protocols = ["http"]
                    host = "127.0.0.1"
                    ports = [{}]
                    "#,
                    address.port()
                ),
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap();
        let registry = crate::runtime::registry::RegistryBuilder::default().build().unwrap();
        let engine = crate::runtime::engine::Engine::with_json(&registry, flows, Some(cfg)).unwrap();

        let response =
            complete_for_engine(&engine, "0000000000000001", None, "system", "prompt", 256, Duration::from_secs(2))
                .await
                .unwrap();

        assert_eq!(response, "draft");
        task.abort();
    }
}
