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
    #[serde(default)]
    pub help: String,
    /// Plugin configuration fields. Not implemented in this prototype: must be empty.
    #[serde(default)]
    pub config: Vec<serde_json::Value>,
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
        plain_text("node.help", &n.help, 4096)?;
        if !n.config.is_empty() {
            return Err(bad("node.config", "plugin configuration is not implemented in this prototype"));
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
output_labels = ["out"]

[[selftest]]
input = { payload = "a" }
expect_outputs = [1]
"##;

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
            ("expect_outputs = [1]", "expect_outputs = [1, 0]", "expect_outputs"),
            ("memory_pages = 4", "memory_page = 4", "unknown field"),
            (
                "output_labels = [\"out\"]",
                "output_labels = [\"out\"]\n[[node.config]]\nname = \"d\"",
                "not implemented",
            ),
        ] {
            let text = SAMPLE.replacen(from, to, 1);
            let err = Manifest::parse(&text).unwrap_err().to_string();
            assert!(err.contains(needle), "{to}: {err}");
        }
    }
}
