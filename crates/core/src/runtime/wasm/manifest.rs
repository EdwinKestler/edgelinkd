//! Manifest schema 1 (DESIGN.md §4). Every table rejects unknown keys, so a misspelled option
//! is an error rather than an ignored setting.

use serde::{Deserialize, Serialize};

use crate::EdgelinkError;

pub(crate) const SUPPORTED_ABI: u32 = 1;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub plugin: PluginMeta,
    #[serde(default)]
    pub limits: LimitRequest,
    pub node: NodeSpec,
    #[serde(default)]
    pub selftest: Vec<SelfTest>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PluginMeta {
    pub id: String,
    pub version: String,
    pub abi: u32,
    pub license: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LimitRequest {
    pub memory_pages: Option<u32>,
    pub fuel_per_message: Option<u64>,
    pub deadline_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NodeSpec {
    pub label: String,
    pub category: String,
    pub color: String,
    pub icon: String,
    pub inputs: u8,
    pub outputs: u8,
    #[serde(default)]
    pub output_labels: Vec<String>,
    /// Advisory `msg.payload` type per output for Flow Copilot (see
    /// [`crate::runtime::nodes::PAYLOAD_TYPES`]); empty means `any` for every output.
    #[serde(default)]
    pub output_payloads: Vec<String>,
    /// Advisory `msg.payload` type the node expects; absent means `any`.
    #[serde(default)]
    pub input_payload: Option<String>,
    #[serde(default)]
    pub help: String,
    /// Plugin configuration fields, validated per node and passed to `el_init` as one object.
    #[serde(default)]
    pub config: Vec<ConfigField>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ConfigKind {
    String,
    Number,
    Boolean,
    Enum,
}

/// One `[[node.config]]` entry. Kind-specific keys (`max_len`; `min`, `max`, `integer`;
/// `values`) are rejected on the other kinds.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigField {
    pub name: String,
    pub kind: ConfigKind,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_len: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default)]
    pub integer: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
}

pub(crate) const MAX_CONFIG_FIELDS: usize = 32;
pub(crate) const MAX_STRING_CONFIG: u32 = 4096;

/// Node properties Node-RED owns; a config field may not shadow them.
pub(crate) const RESERVED_CONFIG_NAMES: &[&str] =
    &["id", "type", "z", "g", "x", "y", "l", "d", "name", "wires", "info", "credentials", "wasmPlugin"];

impl ConfigField {
    /// Check one node value and return its normalised form. The editor stores numbers typed
    /// into text inputs as strings, so numeric strings are accepted for `number`.
    pub(crate) fn check(&self, value: &serde_json::Value) -> Result<serde_json::Value, String> {
        use serde_json::Value;
        match self.kind {
            ConfigKind::String => {
                let text = value.as_str().ok_or("must be a string")?;
                let max = self.max_len.unwrap_or(MAX_STRING_CONFIG) as usize;
                if text.len() > max {
                    return Err(format!("longer than {max} bytes"));
                }
                if text.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
                    return Err("contains control characters".to_owned());
                }
                Ok(value.clone())
            }
            ConfigKind::Number => {
                let number = match value {
                    Value::Number(n) => n.as_f64(),
                    Value::String(text) => text.trim().parse::<f64>().ok(),
                    _ => None,
                }
                .filter(|n| n.is_finite())
                .ok_or("must be a finite number")?;
                if self.integer && number.fract() != 0.0 {
                    return Err("must be an integer".to_owned());
                }
                if self.min.is_some_and(|min| number < min) || self.max.is_some_and(|max| number > max) {
                    return Err(format!(
                        "must be within {}..={}",
                        self.min.map_or("-inf".to_owned(), |v| v.to_string()),
                        self.max.map_or("inf".to_owned(), |v| v.to_string())
                    ));
                }
                if self.integer && number.abs() < 9.0e15 {
                    return Ok(Value::from(number as i64));
                }
                serde_json::Number::from_f64(number).map(Value::Number).ok_or_else(|| "must be finite".to_owned())
            }
            ConfigKind::Boolean => match value {
                Value::Bool(_) => Ok(value.clone()),
                Value::String(text) if text == "true" || text == "false" => Ok(Value::Bool(text == "true")),
                _ => Err("must be true or false".to_owned()),
            },
            ConfigKind::Enum => {
                let text = value.as_str().ok_or("must be a string")?;
                if self.values.iter().any(|v| v == text) {
                    Ok(value.clone())
                } else {
                    Err(format!("must be one of {:?}", self.values))
                }
            }
        }
    }

    fn validate(&self, i: usize) -> crate::Result<()> {
        let field = format!("node.config[{i}]");
        let name_ok = self.name.len() <= 32
            && self.name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && self.name.chars().all(|c| c.is_ascii_alphanumeric());
        if !name_ok {
            return Err(bad(&format!("{field}.name"), "must match [a-z][a-zA-Z0-9]{0,31}"));
        }
        if RESERVED_CONFIG_NAMES.contains(&self.name.as_str()) {
            return Err(bad(&format!("{field}.name"), format!("'{}' is a reserved node property", self.name)));
        }
        plain_text(&format!("{field}.label"), &self.label, 64)?;
        let string = self.kind == ConfigKind::String;
        let number = self.kind == ConfigKind::Number;
        if self.max_len.is_some() && !string {
            return Err(bad(&format!("{field}.max_len"), "only for kind = \"string\""));
        }
        if self.max_len.is_some_and(|m| m == 0 || m > MAX_STRING_CONFIG) {
            return Err(bad(&format!("{field}.max_len"), format!("must be 1..={MAX_STRING_CONFIG}")));
        }
        if (self.min.is_some() || self.max.is_some() || self.integer) && !number {
            return Err(bad(&field, "min, max and integer are only for kind = \"number\""));
        }
        if self.min.is_some_and(|v| !v.is_finite()) || self.max.is_some_and(|v| !v.is_finite()) {
            return Err(bad(&field, "min and max must be finite"));
        }
        if let (Some(min), Some(max)) = (self.min, self.max)
            && min > max
        {
            return Err(bad(&field, "min is greater than max"));
        }
        if self.kind == ConfigKind::Enum {
            if self.values.is_empty() || self.values.len() > 32 {
                return Err(bad(&format!("{field}.values"), "needs 1-32 values"));
            }
            for (j, value) in self.values.iter().enumerate() {
                plain_text(&format!("{field}.values"), value, 64)?;
                if self.values[..j].contains(value) {
                    return Err(bad(&format!("{field}.values"), format!("'{value}' is listed twice")));
                }
            }
        } else if !self.values.is_empty() {
            return Err(bad(&format!("{field}.values"), "only for kind = \"enum\""));
        }
        if let Some(default) = &self.default {
            self.check(default).map_err(|why| bad(&format!("{field}.default"), why))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SelfTest {
    pub input: serde_json::Value,
    pub expect_outputs: Vec<u32>,
}

fn bad(field: &str, why: impl std::fmt::Display) -> EdgelinkError {
    EdgelinkError::NotSupported(format!("manifest {field}: {why}"))
}

/// `[a-z][a-z0-9]{0,31}`: no dashes, so `wasm-<publisher>-<name>` is injective.
pub(crate) fn valid_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    matches!(chars.next(), Some('a'..='z'))
        && segment.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

pub(crate) fn split_id(id: &str) -> crate::Result<(&str, &str)> {
    id.split_once('/').filter(|(p, n)| valid_segment(p) && valid_segment(n)).ok_or_else(|| {
        bad("plugin.id", format!("'{id}' is not <publisher>/<name> with [a-z][a-z0-9]{{0,31}} segments"))
    })
}

fn plain_text(field: &str, text: &str, max: usize) -> crate::Result<()> {
    if text.len() > max {
        return Err(bad(field, format!("longer than {max} bytes")));
    }
    if text.chars().any(|c| c.is_control() && c != '\n') {
        return Err(bad(field, "contains control characters"));
    }
    Ok(())
}

impl Manifest {
    pub(crate) fn parse(text: &str) -> crate::Result<Self> {
        let manifest: Manifest =
            toml_edit::de::from_str(text).map_err(|err| EdgelinkError::NotSupported(format!("manifest: {err}")))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub(crate) fn version(&self) -> semver::Version {
        semver::Version::parse(&self.plugin.version).expect("validated")
    }

    fn validate(&self) -> crate::Result<()> {
        let p = &self.plugin;
        split_id(&p.id)?;
        let version = semver::Version::parse(&p.version).map_err(|err| bad("plugin.version", err))?;
        if !version.build.is_empty() {
            return Err(bad("plugin.version", "build metadata is not allowed"));
        }
        if p.abi != SUPPORTED_ABI {
            return Err(bad(
                "plugin.abi",
                format!("ABI {} is not supported (this runtime supports {SUPPORTED_ABI})", p.abi),
            ));
        }
        if p.license.is_empty() || p.license.len() > 64 || !p.license.chars().all(|c| c.is_ascii_graphic() || c == ' ')
        {
            return Err(bad("plugin.license", "must be 1-64 printable ASCII characters"));
        }
        plain_text("plugin.description", &p.description, 256)?;
        if let Some(cap) = p.capabilities.first() {
            return Err(bad("plugin.capabilities", format!("unsupported capability '{cap}' in ABI 1")));
        }
        let n = &self.node;
        plain_text("node.label", &n.label, 64)?;
        if n.label.is_empty() {
            return Err(bad("node.label", "must not be empty"));
        }
        let category_ok = n.category.len() <= 32
            && n.category.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && n.category.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || " _-".contains(c));
        if !category_ok {
            return Err(bad("node.category", "must match [a-z][a-z0-9 _-]{0,31}"));
        }
        let color_ok =
            n.color.len() == 7 && n.color.starts_with('#') && n.color[1..].chars().all(|c| c.is_ascii_hexdigit());
        if !color_ok {
            return Err(bad("node.color", "must be #RRGGBB"));
        }
        let icon_ok = n.icon.len() <= 64
            && (n.icon.ends_with(".svg") || n.icon.ends_with(".png"))
            && n.icon.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c));
        if !icon_ok {
            return Err(bad("node.icon", "must name a bundled Node-RED icon such as \"function.svg\""));
        }
        if n.inputs != 1 {
            return Err(bad("node.inputs", "must be 1 (ABI 1 has no trigger for input-less nodes)"));
        }
        if n.outputs > 16 {
            return Err(bad("node.outputs", "at most 16"));
        }
        if !n.output_labels.is_empty() && n.output_labels.len() != n.outputs as usize {
            return Err(bad(
                "node.output_labels",
                format!("has {} labels for {} outputs", n.output_labels.len(), n.outputs),
            ));
        }
        for label in &n.output_labels {
            plain_text("node.output_labels", label, 64)?;
        }
        if !n.output_payloads.is_empty() && n.output_payloads.len() != n.outputs as usize {
            return Err(bad(
                "node.output_payloads",
                format!("has {} types for {} outputs", n.output_payloads.len(), n.outputs),
            ));
        }
        let payload_rule =
            "must be one or more of any, string, number, boolean, object, array, buffer, null joined with '|'";
        for payload in &n.output_payloads {
            if !crate::runtime::nodes::valid_payload_type(payload) {
                return Err(bad("node.output_payloads", format!("'{payload}' {payload_rule}")));
            }
        }
        if let Some(payload) = &n.input_payload
            && !crate::runtime::nodes::valid_payload_type(payload)
        {
            return Err(bad("node.input_payload", format!("'{payload}' {payload_rule}")));
        }
        plain_text("node.help", &n.help, 4096)?;
        if n.config.len() > MAX_CONFIG_FIELDS {
            return Err(bad("node.config", format!("at most {MAX_CONFIG_FIELDS} fields")));
        }
        for (i, field) in n.config.iter().enumerate() {
            field.validate(i)?;
            if n.config[..i].iter().any(|other| other.name == field.name) {
                return Err(bad("node.config", format!("field '{}' is declared twice", field.name)));
            }
            if field.required && field.default.is_none() && !self.selftest.is_empty() {
                return Err(bad(
                    "node.config",
                    format!("self-tests run with defaults, so required field '{}' needs a default", field.name),
                ));
            }
        }
        if self.selftest.len() > 4 {
            return Err(bad("selftest", "at most 4 vectors"));
        }
        for (i, test) in self.selftest.iter().enumerate() {
            if !test.input.is_object() {
                return Err(bad(&format!("selftest[{i}].input"), "must be a table (the message)"));
            }
            if test.expect_outputs.len() != n.outputs as usize {
                return Err(bad(
                    &format!("selftest[{i}].expect_outputs"),
                    format!("has {} entries for {} outputs", test.expect_outputs.len(), n.outputs),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) const SAMPLE: &str = r##"
[plugin]
id = "acme/upper"
version = "1.2.0"
abi = 1
license = "MIT"
description = "Upper-cases payloads"

[limits]
memory_pages = 4

[node]
label = "upper"
category = "plugins"
color = "#C0DEED"
icon = "function.svg"
inputs = 1
outputs = 1
output_payloads = ["string"]
input_payload = "string|buffer"
output_labels = ["out"]

[[selftest]]
input = { payload = "a" }
expect_outputs = [1]
"##;

/// A manifest for raw test modules: one input, `outputs` outputs, no self-tests.
#[cfg(test)]
pub(crate) fn synthetic(id: &str, version: &semver::Version, outputs: u8, config: Vec<ConfigField>) -> Manifest {
    Manifest {
        plugin: PluginMeta {
            id: id.to_owned(),
            version: version.to_string(),
            abi: SUPPORTED_ABI,
            license: "MIT".to_owned(),
            description: String::new(),
            capabilities: Vec::new(),
        },
        limits: LimitRequest::default(),
        node: NodeSpec {
            label: id.rsplit('/').next().unwrap_or(id).to_owned(),
            category: "plugins".to_owned(),
            color: "#C0DEED".to_owned(),
            icon: "function.svg".to_owned(),
            inputs: 1,
            outputs,
            output_labels: Vec::new(),
            output_payloads: Vec::new(),
            input_payload: None,
            help: String::new(),
            config,
        },
        selftest: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_parses() {
        let m = Manifest::parse(SAMPLE).unwrap();
        assert_eq!(m.plugin.id, "acme/upper");
        assert_eq!(m.version(), semver::Version::new(1, 2, 0));
        assert_eq!(m.limits.memory_pages, Some(4));
        assert_eq!(m.selftest.len(), 1);
        assert_eq!(m.node.output_payloads, ["string"]);
        assert_eq!(m.node.input_payload.as_deref(), Some("string|buffer"));
    }

    #[test]
    fn config_fields_validate_and_normalise_values() {
        let text = SAMPLE.replacen(
            "output_labels = [\"out\"]",
            r#"output_labels = ["out"]
[[node.config]]
name = "delimiter"
kind = "string"
max_len = 1
default = ","
[[node.config]]
name = "limit"
kind = "number"
integer = true
min = 1.0
max = 10.0
default = 5
[[node.config]]
name = "header"
kind = "boolean"
default = true
[[node.config]]
name = "mode"
kind = "enum"
values = ["fast", "safe"]
default = "safe"
"#,
            1,
        );
        let m = Manifest::parse(&text).unwrap();
        let [delimiter, limit, header, mode] = &m.node.config[..] else { panic!("4 fields") };
        use serde_json::json;
        assert_eq!(delimiter.check(&json!(";")), Ok(json!(";")));
        assert!(delimiter.check(&json!(";;")).is_err());
        assert!(delimiter.check(&json!(1)).is_err());
        assert_eq!(limit.check(&json!("7")), Ok(json!(7)));
        assert!(limit.check(&json!(7.5)).unwrap_err().contains("integer"));
        assert!(limit.check(&json!(11)).unwrap_err().contains("within"));
        assert_eq!(header.check(&json!("false")), Ok(json!(false)));
        assert!(header.check(&json!(1)).is_err());
        assert_eq!(mode.check(&json!("fast")), Ok(json!("fast")));
        assert!(mode.check(&json!("slow")).unwrap_err().contains("one of"));
    }

    #[test]
    fn every_rule_fails_loudly() {
        for (from, to, needle) in [
            ("id = \"acme/upper\"", "id = \"acme-x/upper\"", "plugin.id"),
            ("id = \"acme/upper\"", "id = \"../upper\"", "plugin.id"),
            ("version = \"1.2.0\"", "version = \"one\"", "plugin.version"),
            ("abi = 1", "abi = 2", "ABI 2 is not supported"),
            ("license = \"MIT\"", "license = \"\"", "plugin.license"),
            (
                "description = \"Upper-cases payloads\"",
                "capabilities = [\"net:http\"]",
                "unsupported capability 'net:http'",
            ),
            ("color = \"#C0DEED\"", "color = \"red\"", "node.color"),
            ("icon = \"function.svg\"", "icon = \"../x.svg\"", "node.icon"),
            ("inputs = 1", "inputs = 0", "node.inputs"),
            ("output_labels = [\"out\"]", "output_labels = [\"a\", \"b\"]", "node.output_labels"),
            ("output_payloads = [\"string\"]", "output_payloads = [\"string\", \"any\"]", "node.output_payloads"),
            ("output_payloads = [\"string\"]", "output_payloads = [\"text\"]", "node.output_payloads"),
            ("input_payload = \"string|buffer\"", "input_payload = \"any|string\"", "node.input_payload"),
            ("expect_outputs = [1]", "expect_outputs = [1, 0]", "expect_outputs"),
            ("memory_pages = 4", "memory_page = 4", "unknown field"),
            (
                "output_labels = [\"out\"]",
                "output_labels = [\"out\"]\n[[node.config]]\nname = \"wires\"\nkind = \"string\"",
                "reserved node property",
            ),
            (
                "output_labels = [\"out\"]",
                "output_labels = [\"out\"]\n[[node.config]]\nname = \"Bad\"\nkind = \"string\"",
                "node.config[0].name",
            ),
            (
                "output_labels = [\"out\"]",
                "output_labels = [\"out\"]\n[[node.config]]\nname = \"n\"\nkind = \"string\"\nmin = 1.0",
                "only for kind = \"number\"",
            ),
            (
                "output_labels = [\"out\"]",
                "output_labels = [\"out\"]\n[[node.config]]\nname = \"n\"\nkind = \"number\"\nmax = 3.0\ndefault = 4",
                "node.config[0].default",
            ),
            (
                "output_labels = [\"out\"]",
                "output_labels = [\"out\"]\n[[node.config]]\nname = \"e\"\nkind = \"enum\"",
                "needs 1-32 values",
            ),
            (
                "output_labels = [\"out\"]",
                "output_labels = [\"out\"]\n[[node.config]]\nname = \"n\"\nkind = \"string\"\n[[node.config]]\nname = \"n\"\nkind = \"boolean\"",
                "declared twice",
            ),
            (
                "output_labels = [\"out\"]",
                "output_labels = [\"out\"]\n[[node.config]]\nname = \"n\"\nkind = \"string\"\nrequired = true",
                "needs a default",
            ),
            (
                "output_labels = [\"out\"]",
                "output_labels = [\"out\"]\n[[node.config]]\nname = \"n\"\nkind = \"color\"",
                "unknown variant",
            ),
        ] {
            let text = SAMPLE.replacen(from, to, 1);
            let err = Manifest::parse(&text).unwrap_err().to_string();
            assert!(err.contains(needle), "{to}: {err}");
        }
    }
}
