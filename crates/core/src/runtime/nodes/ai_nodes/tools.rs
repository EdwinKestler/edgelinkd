//! Closed agent tool enum.

use serde_json::json;

use super::adapter::ToolSpec;
use crate::EdgelinkError;

pub(crate) const CONTEXT_GET: &str = "context_get";
pub(crate) const CONTEXT_SET: &str = "context_set";

const FORBIDDEN: &[&str] = &["exec", "shell", "file", "http", "mqtt", "deploy", "flows", "network", "eval", "js"];

pub(crate) fn context_get_spec() -> ToolSpec {
    ToolSpec {
        name: CONTEXT_GET,
        description: "Read a context key",
        parameters: json!({
            "type": "object",
            "properties": {
                "scope": { "type": "string", "enum": ["node", "flow", "global"] },
                "key": { "type": "string", "minLength": 1, "maxLength": 64 }
            },
            "required": ["scope", "key"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn context_set_spec() -> ToolSpec {
    ToolSpec {
        name: CONTEXT_SET,
        description: "Write a context key",
        parameters: json!({
            "type": "object",
            "properties": {
                "scope": { "type": "string", "enum": ["node", "flow", "global"] },
                "key": { "type": "string", "minLength": 1, "maxLength": 64 },
                "value": {}
            },
            "required": ["scope", "key", "value"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn spec_for(name: &str) -> crate::Result<ToolSpec> {
    validate_tool_name(name)?;
    match name {
        CONTEXT_GET => Ok(context_get_spec()),
        CONTEXT_SET => Ok(context_set_spec()),
        _ => Err(EdgelinkError::NotSupported(format!("ai-agent tool '{name}' is not supported"))),
    }
}

pub(crate) fn validate_tool_name(name: &str) -> crate::Result<()> {
    if FORBIDDEN.contains(&name) || name.contains('.') {
        return Err(EdgelinkError::NotSupported(format!("ai-agent tool '{name}' is not supported")));
    }
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(EdgelinkError::NotSupported(format!("ai-agent tool '{name}' is not supported")));
    }
    Ok(())
}

pub(crate) fn valid_context_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64 && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
