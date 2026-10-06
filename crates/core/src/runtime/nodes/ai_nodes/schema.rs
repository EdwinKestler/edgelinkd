//! Draft-07 subset. Unknown keywords fail at compile. No extra crate.

use serde_json::Value;

use crate::N2linkError;

const MAX_SCHEMA_BYTES: usize = 32 * 1024;
const MAX_DEPTH: usize = 8;
const MAX_PROPERTIES: usize = 64;
const MAX_ENUM: usize = 64;
const MAX_ENUM_STRING: usize = 256;

const ALLOW: &[&str] = &[
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
];
const IGNORE: &[&str] = &["$schema", "$id", "$comment", "title", "description", "default", "examples"];

#[derive(Debug, Clone)]
pub(crate) struct CompiledSchema {
    raw: Value,
}

impl CompiledSchema {
    pub(crate) fn compile(schema: &Value) -> crate::Result<Self> {
        let bytes = serde_json::to_vec(schema).map_err(|err| N2linkError::invalid_operation(&err.to_string()))?;
        if bytes.len() > MAX_SCHEMA_BYTES {
            return Err(N2linkError::invalid_operation("schema exceeds 32 KiB"));
        }
        walk_compile(schema, 0)?;
        Ok(Self { raw: schema.clone() })
    }

    pub(crate) fn validate(&self, value: &Value) -> crate::Result<()> {
        validate_against(&self.raw, value)
    }
}

fn walk_compile(schema: &Value, depth: usize) -> crate::Result<()> {
    if depth > MAX_DEPTH {
        return Err(N2linkError::invalid_operation("schema nesting exceeds 8"));
    }
    let object = schema.as_object().ok_or_else(|| N2linkError::invalid_operation("schema must be an object"))?;
    for key in object.keys() {
        if ALLOW.contains(&key.as_str()) || IGNORE.contains(&key.as_str()) {
            continue;
        }
        return Err(N2linkError::NotSupported(format!("JSON Schema keyword '{key}' is not supported")));
    }
    if let Some(additional) = object.get("additionalProperties")
        && !additional.is_boolean()
    {
        return Err(N2linkError::NotSupported("additionalProperties as a nested schema is not supported".to_owned()));
    }
    if let Some(properties) = object.get("properties") {
        let map =
            properties.as_object().ok_or_else(|| N2linkError::invalid_operation("properties must be an object"))?;
        if map.len() > MAX_PROPERTIES {
            return Err(N2linkError::invalid_operation("schema properties exceed 64"));
        }
        for child in map.values() {
            walk_compile(child, depth + 1)?;
        }
    }
    if let Some(items) = object.get("items") {
        walk_compile(items, depth + 1)?;
    }
    if let Some(enum_values) = object.get("enum") {
        let list = enum_values.as_array().ok_or_else(|| N2linkError::invalid_operation("enum must be an array"))?;
        if list.len() > MAX_ENUM {
            return Err(N2linkError::invalid_operation("enum exceeds 64 values"));
        }
        for item in list {
            if let Some(text) = item.as_str()
                && text.len() > MAX_ENUM_STRING
            {
                return Err(N2linkError::invalid_operation("enum string exceeds 256 characters"));
            }
        }
    }
    Ok(())
}

fn validate_against(schema: &Value, value: &Value) -> crate::Result<()> {
    let object = schema.as_object().ok_or_else(|| N2linkError::invalid_operation("schema must be an object"))?;
    if let Some(type_value) = object.get("type") {
        check_type(type_value, value)?;
    }
    if let Some(const_value) = object.get("const")
        && const_value != value
    {
        return Err(N2linkError::invalid_operation("value does not match const"));
    }
    if let Some(enum_values) = object.get("enum") {
        let list = enum_values.as_array().ok_or_else(|| N2linkError::invalid_operation("enum must be an array"))?;
        if !list.iter().any(|item| item == value) {
            return Err(N2linkError::invalid_operation("value is not in enum"));
        }
    }
    if let Some(min) = object.get("minLength").and_then(Value::as_u64) {
        let text = value.as_str().ok_or_else(|| N2linkError::invalid_operation("minLength requires a string"))?;
        if (text.chars().count() as u64) < min {
            return Err(N2linkError::invalid_operation("string is shorter than minLength"));
        }
    }
    if let Some(max) = object.get("maxLength").and_then(Value::as_u64) {
        let text = value.as_str().ok_or_else(|| N2linkError::invalid_operation("maxLength requires a string"))?;
        if (text.chars().count() as u64) > max {
            return Err(N2linkError::invalid_operation("string is longer than maxLength"));
        }
    }
    if let Some(min) = object.get("minimum").and_then(Value::as_f64) {
        let number = value.as_f64().ok_or_else(|| N2linkError::invalid_operation("minimum requires a number"))?;
        if number < min {
            return Err(N2linkError::invalid_operation("number is below minimum"));
        }
    }
    if let Some(max) = object.get("maximum").and_then(Value::as_f64) {
        let number = value.as_f64().ok_or_else(|| N2linkError::invalid_operation("maximum requires a number"))?;
        if number > max {
            return Err(N2linkError::invalid_operation("number is above maximum"));
        }
    }
    if let Some(min) = object.get("minItems").and_then(Value::as_u64) {
        let list = value.as_array().ok_or_else(|| N2linkError::invalid_operation("minItems requires an array"))?;
        if (list.len() as u64) < min {
            return Err(N2linkError::invalid_operation("array is shorter than minItems"));
        }
    }
    if let Some(max) = object.get("maxItems").and_then(Value::as_u64) {
        let list = value.as_array().ok_or_else(|| N2linkError::invalid_operation("maxItems requires an array"))?;
        if (list.len() as u64) > max {
            return Err(N2linkError::invalid_operation("array is longer than maxItems"));
        }
    }
    if let Some(properties) = object.get("properties") {
        let map = value.as_object().ok_or_else(|| N2linkError::invalid_operation("properties requires an object"))?;
        let defs =
            properties.as_object().ok_or_else(|| N2linkError::invalid_operation("properties must be an object"))?;
        for (name, child) in defs {
            if let Some(item) = map.get(name) {
                validate_against(child, item)?;
            }
        }
        if object.get("additionalProperties") == Some(&Value::Bool(false)) {
            for key in map.keys() {
                if !defs.contains_key(key) {
                    return Err(N2linkError::invalid_operation(&format!("unexpected property '{key}'")));
                }
            }
        }
    } else if object.get("additionalProperties") == Some(&Value::Bool(false))
        && let Some(map) = value.as_object()
        && !map.is_empty()
    {
        return Err(N2linkError::invalid_operation("object has additional properties"));
    }
    if let Some(required) = object.get("required") {
        let names = required.as_array().ok_or_else(|| N2linkError::invalid_operation("required must be an array"))?;
        let map = value.as_object().ok_or_else(|| N2linkError::invalid_operation("required requires an object"))?;
        for name in names {
            let key = name.as_str().ok_or_else(|| N2linkError::invalid_operation("required names must be strings"))?;
            if !map.contains_key(key) {
                return Err(N2linkError::invalid_operation(&format!("missing required property '{key}'")));
            }
        }
    }
    if let Some(items) = object.get("items") {
        let list = value.as_array().ok_or_else(|| N2linkError::invalid_operation("items requires an array"))?;
        for item in list {
            validate_against(items, item)?;
        }
    }
    Ok(())
}

fn check_type(type_value: &Value, value: &Value) -> crate::Result<()> {
    match type_value {
        Value::String(name) => type_matches(name, value),
        Value::Array(names) => {
            for name in names {
                let Some(label) = name.as_str() else {
                    return Err(N2linkError::invalid_operation("type array must be strings"));
                };
                if type_matches(label, value).is_ok() {
                    return Ok(());
                }
            }
            Err(N2linkError::invalid_operation("value does not match type"))
        }
        _ => Err(N2linkError::invalid_operation("type must be a string or array")),
    }
}

fn type_matches(name: &str, value: &Value) -> crate::Result<()> {
    let ok = match name {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        other => return Err(N2linkError::NotSupported(format!("JSON Schema type '{other}' is not supported"))),
    };
    if ok { Ok(()) } else { Err(N2linkError::invalid_operation(&format!("value is not {name}"))) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_schema_accepts_any_value() {
        let schema = CompiledSchema::compile(&json!({})).unwrap();
        schema.validate(&json!(1)).unwrap();
        schema.validate(&json!({"a": true})).unwrap();
    }

    #[test]
    fn ref_is_rejected_at_compile() {
        let err = CompiledSchema::compile(&json!({ "$ref": "#/defs/x" })).unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");
    }

    #[test]
    fn schema_annotation_is_ignored() {
        CompiledSchema::compile(&json!({ "$schema": "https://json-schema.org/draft-07/schema#", "type": "string" }))
            .unwrap();
    }

    #[test]
    fn additional_properties_false_rejects_unknown_keys() {
        let schema = CompiledSchema::compile(&json!({
            "type": "object",
            "properties": { "a": { "type": "number" } },
            "additionalProperties": false
        }))
        .unwrap();
        schema.validate(&json!({ "a": 1 })).unwrap();
        assert!(schema.validate(&json!({ "a": 1, "b": 2 })).is_err());
    }
}
