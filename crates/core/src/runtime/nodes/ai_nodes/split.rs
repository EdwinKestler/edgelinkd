//! Local deterministic Unicode-character splitter.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::EdgelinkError;
use crate::runtime::flow::Flow;
use crate::runtime::model::json::RedFlowNodeConfig;
use crate::runtime::model::{MsgHandle, Variant};
use crate::runtime::nodes::*;
use edgelink_macro::*;

const MAX_INPUT_BYTES: usize = 1_048_576;

crate::node_hints!("ai-split", caps = ["ai"]);

#[flow_node("ai-split", red_name = "ai-split", inputs = 1, outputs = 1)]
struct AiSplitNode {
    base: BaseFlowNodeState,
    config: ResolvedSplit,
    counter: AtomicU64,
}

#[derive(Debug, Deserialize)]
struct SplitConfig {
    #[serde(default, rename = "chunkSize", deserialize_with = "empty_as_none_usize")]
    chunk_size: Option<usize>,
    #[serde(default, deserialize_with = "empty_as_none_usize")]
    overlap: Option<usize>,
    #[serde(default)]
    separator: String,
    #[serde(default, rename = "maxChunks", deserialize_with = "empty_as_none_usize")]
    max_chunks: Option<usize>,
    #[serde(default)]
    property: String,
}

struct ResolvedSplit {
    chunk_size: usize,
    overlap: usize,
    separator: String,
    max_chunks: usize,
    property: String,
}

struct Chunk {
    text: String,
    offset: usize,
}

impl AiSplitNode {
    fn build(
        _flow: &Flow,
        base_node: BaseFlowNodeState,
        config: &RedFlowNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        reject_unsupported(&config.rest)?;
        let raw = SplitConfig::deserialize(&config.rest)?;
        let chunk_size = raw.chunk_size.unwrap_or(512);
        if !(1..=8192).contains(&chunk_size) {
            return Err(EdgelinkError::invalid_operation("ai-split chunkSize is out of range"));
        }
        let overlap = raw.overlap.unwrap_or(0);
        if overlap >= chunk_size {
            return Err(EdgelinkError::invalid_operation("ai-split overlap must be less than chunkSize"));
        }
        let separator = raw.separator;
        if separator.chars().count() > 16 {
            return Err(EdgelinkError::invalid_operation("ai-split separator is too long"));
        }
        if !separator.is_empty() && separator.chars().count() >= chunk_size {
            return Err(EdgelinkError::invalid_operation("ai-split separator must be shorter than chunkSize"));
        }
        let max_chunks = raw.max_chunks.unwrap_or(256);
        if !(1..=256).contains(&max_chunks) {
            return Err(EdgelinkError::invalid_operation("ai-split maxChunks is out of range"));
        }
        let property = if raw.property.trim().is_empty() { "payload".to_owned() } else { raw.property };
        Ok(Box::new(AiSplitNode {
            base: base_node,
            config: ResolvedSplit { chunk_size, overlap, separator, max_chunks, property },
            counter: AtomicU64::new(0),
        }))
    }

    async fn handle(&self, msg: MsgHandle, cancel: CancellationToken) -> crate::Result<()> {
        let source = {
            let guard = msg.read().await;
            let value = guard
                .get(&self.config.property)
                .ok_or_else(|| EdgelinkError::invalid_operation("ai-split input is empty"))?;
            let text =
                value.as_str().ok_or_else(|| EdgelinkError::invalid_operation("ai-split input must be a string"))?;
            if text.is_empty() {
                return Err(EdgelinkError::invalid_operation("ai-split input is empty"));
            }
            if text.len() > MAX_INPUT_BYTES {
                return Err(EdgelinkError::invalid_operation("ai-split input exceeds 1 MiB"));
            }
            text.to_owned()
        };
        let chunks = split_text(&source, self.config.chunk_size, self.config.overlap, &self.config.separator)?;
        if chunks.len() > self.config.max_chunks {
            return Err(EdgelinkError::invalid_operation("ai-split maxChunks exceeded"));
        }
        if chunks.is_empty() {
            return Err(EdgelinkError::invalid_operation("ai-split produced no chunks"));
        }
        let msgid = {
            let guard = msg.read().await;
            guard.id().map(|id| id.to_string())
        };
        let parts_id = match msgid {
            Some(id) => format!("{}:{id}", self.id()),
            None => format!("{}:{}", self.id(), self.counter.fetch_add(1, Ordering::Relaxed)),
        };
        let count = chunks.len();
        let mut tails = Vec::with_capacity(count.saturating_sub(1));
        for _ in chunks.iter().skip(1) {
            tails.push(msg.deep_clone(false).await);
        }
        write_chunk(&msg, &self.config.property, &chunks[0], 0, count, &parts_id).await;
        for (index, (handle, chunk)) in tails.into_iter().zip(chunks.iter().skip(1)).enumerate() {
            write_chunk(&handle, &self.config.property, chunk, index + 1, count, &parts_id).await;
            self.fan_out_one(Envelope { port: 0, msg: handle }, cancel.clone()).await?;
        }
        self.report_status(
            StatusObject { fill: Some(StatusFill::Green), shape: Some(StatusShape::Dot), text: Some("ok".to_owned()) },
            cancel.clone(),
        )
        .await;
        self.fan_out_one(Envelope { port: 0, msg }, cancel).await
    }
}

async fn write_chunk(msg: &MsgHandle, property: &str, chunk: &Chunk, index: usize, count: usize, parts_id: &str) {
    let mut guard = msg.write().await;
    guard.set(property.to_owned(), Variant::String(chunk.text.clone()));
    let mut parts = crate::runtime::model::VariantObjectMap::new();
    parts.insert("id".to_owned(), Variant::String(parts_id.to_owned()));
    parts.insert("type".to_owned(), Variant::String("string".to_owned()));
    parts.insert("index".to_owned(), Variant::from(index as u64));
    parts.insert("count".to_owned(), Variant::from(count as u64));
    guard.set("parts".to_owned(), Variant::Object(parts));
    let mut ai = crate::runtime::model::VariantObjectMap::new();
    let mut meta = crate::runtime::model::VariantObjectMap::new();
    meta.insert("index".to_owned(), Variant::from(index as u64));
    meta.insert("count".to_owned(), Variant::from(count as u64));
    meta.insert("offset".to_owned(), Variant::from(chunk.offset as u64));
    ai.insert("chunk".to_owned(), Variant::Object(meta));
    guard.set("ai".to_owned(), Variant::Object(ai));
}

fn split_text(source: &str, chunk_size: usize, overlap: usize, separator: &str) -> crate::Result<Vec<Chunk>> {
    if source.is_empty() {
        return Err(EdgelinkError::invalid_operation("ai-split input is empty"));
    }
    if separator.is_empty() {
        return Ok(window_chars(&source.chars().collect::<Vec<_>>(), chunk_size, overlap, 0));
    }
    pack_segments(source, chunk_size, overlap, separator)
}

fn window_chars(chars: &[char], chunk_size: usize, overlap: usize, base_offset: usize) -> Vec<Chunk> {
    let mut out = Vec::new();
    if chars.is_empty() {
        return out;
    }
    let step = chunk_size.saturating_sub(overlap).max(1);
    let mut i = 0;
    while i < chars.len() {
        let end = (i + chunk_size).min(chars.len());
        let text: String = chars[i..end].iter().collect();
        if !text.is_empty() {
            out.push(Chunk { text, offset: base_offset + i });
        }
        if end == chars.len() {
            break;
        }
        i += step;
    }
    out
}

fn pack_segments(source: &str, chunk_size: usize, overlap: usize, separator: &str) -> crate::Result<Vec<Chunk>> {
    let segments: Vec<&str> = source.split(separator).collect();
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_offset = 0usize;
    let mut cursor = 0usize;
    let source_chars: Vec<char> = source.chars().collect();
    for (index, seg) in segments.iter().enumerate() {
        let seg_offset = cursor;
        cursor += seg.chars().count();
        if index + 1 < segments.len() {
            cursor += separator.chars().count();
        }
        let seg_len = seg.chars().count();
        if seg_len > chunk_size {
            if !current.is_empty() {
                chunks.push(Chunk { text: std::mem::take(&mut current), offset: current_offset });
            }
            let hard = window_chars(&seg.chars().collect::<Vec<_>>(), chunk_size, overlap, seg_offset);
            chunks.extend(hard);
            continue;
        }
        let candidate = if current.is_empty() { (*seg).to_owned() } else { format!("{current}{separator}{seg}") };
        if candidate.chars().count() <= chunk_size {
            if current.is_empty() {
                current_offset = seg_offset;
            }
            current = candidate;
        } else {
            if !current.is_empty() {
                chunks.push(Chunk { text: std::mem::take(&mut current), offset: current_offset });
            }
            current = (*seg).to_owned();
            current_offset = seg_offset;
        }
    }
    if !current.is_empty() {
        chunks.push(Chunk { text: current, offset: current_offset });
    }
    let _ = source_chars;
    Ok(chunks.into_iter().filter(|chunk| !chunk.text.is_empty()).collect())
}

fn reject_unsupported(value: &Value) -> crate::Result<()> {
    for key in ["stream", "tiktoken", "model", "provider", "tokens"] {
        if value
            .get(key)
            .is_some_and(|item| !item.is_null() && item != &Value::Bool(false) && item != &Value::String(String::new()))
        {
            return Err(EdgelinkError::NotSupported(format!("AI option '{key}' is not supported")));
        }
    }
    Ok(())
}

fn empty_as_none_usize<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => text.parse().map(Some).map_err(serde::de::Error::custom),
        Some(Value::Number(number)) => {
            number.as_u64().map(|n| Some(n as usize)).ok_or_else(|| serde::de::Error::custom("not a number"))
        }
        Some(_) => Err(serde::de::Error::custom("not a number")),
    }
}

#[async_trait::async_trait]
impl FlowNodeBehavior for AiSplitNode {
    fn get_base(&self) -> &BaseFlowNodeState {
        &self.base
    }

    async fn run(self: Arc<Self>, stop_token: CancellationToken) {
        while !stop_token.is_cancelled() {
            let cancel = stop_token.child_token();
            with_uow(self.as_ref(), cancel.child_token(), |node, msg| async move { node.handle(msg, cancel).await })
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    fn texts(source: &str, size: usize, overlap: usize, sep: &str) -> Vec<String> {
        split_text(source, size, overlap, sep).unwrap().into_iter().map(|c| c.text).collect()
    }

    #[test]
    fn window_table() {
        assert_eq!(texts("abcdef", 4, 0, ""), vec!["abcd", "ef"]);
        assert_eq!(texts("abcdef", 4, 2, ""), vec!["abcd", "cdef"]);
        assert_eq!(texts("abcdefgh", 3, 1, ""), vec!["abc", "cde", "efg", "gh"]);
        assert_eq!(texts("ab\ncd\nef", 5, 0, "\n"), vec!["ab\ncd", "ef"]);
        assert_eq!(texts("xxxx", 3, 0, "yy"), vec!["xxx", "x"]);
        assert_eq!(texts("aa--bbbb--c", 3, 1, "--"), vec!["aa", "bbb", "bb", "c"]);
    }

    #[tokio::test]
    async fn split_emits_two_chunks_with_a_shared_parts_id() {
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "1", "z": "100", "type": "ai-split", "chunkSize": 4, "overlap": 0, "wires": [["2"]] },
            { "id": "2", "z": "100", "type": "test-once" }
        ]);
        let engine = crate::runtime::engine::build_test_engine(flows).unwrap();
        let injected: Vec<(crate::runtime::model::ElementId, crate::runtime::model::Msg)> =
            Vec::deserialize(json!([["1", { "payload": "abcdef" }]])).unwrap();
        let msgs = engine.run_once_with_inject(2, std::time::Duration::from_secs(2), injected).await.unwrap();
        assert_eq!(msgs.len(), 2);
        let mut payloads: Vec<_> = msgs.iter().map(|m| m.get("payload").and_then(Variant::as_str)).collect();
        payloads.sort();
        assert_eq!(payloads, vec![Some("abcd"), Some("ef")]);
        let id0 = msgs[0].get("parts").and_then(Variant::as_object).and_then(|o| o.get("id")).and_then(Variant::as_str);
        let id1 = msgs[1].get("parts").and_then(Variant::as_object).and_then(|o| o.get("id")).and_then(Variant::as_str);
        assert_eq!(id0, id1);
        assert!(id0.is_some());
    }
}
