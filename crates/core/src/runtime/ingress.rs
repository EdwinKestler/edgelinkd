//! Shared inbound HTTP resource limits.
//!
//! The web crate applies these limits to the Node-RED-compatible API. The `http in` node
//! applies the webhook class directly because it owns a separate TCP listener.

use std::net::IpAddr;
use std::str::FromStr;
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProtectionMode {
    Off,
    Observe,
    #[default]
    Enforce,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EndpointClass {
    Health,
    EditorAdmin,
    Authentication,
    Websocket,
    Webhook,
    Copilot,
    Fleet,
    /// `/wasm/plugins…` (plugin install and activation).
    Plugins,
    Static,
}

impl EndpointClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Health => "health",
            Self::EditorAdmin => "editor_admin",
            Self::Authentication => "authentication",
            Self::Websocket => "websocket",
            Self::Webhook => "webhook",
            Self::Copilot => "copilot",
            Self::Fleet => "fleet",
            Self::Plugins => "plugins",
            Self::Static => "static",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EndpointLimits {
    pub mode: ProtectionMode,
    pub max_body_bytes: usize,
    pub max_header_bytes: usize,
    pub max_headers: usize,
    pub requests_per_minute: u32,
    pub max_concurrency: usize,
    pub queue_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub max_response_bytes: usize,
}

impl EndpointLimits {
    pub fn queue_timeout(&self) -> Duration {
        Duration::from_millis(self.queue_timeout_ms)
    }

    pub fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.request_timeout_ms)
    }

    fn validate(&self, class: EndpointClass) -> Result<(), String> {
        if self.mode == ProtectionMode::Off {
            return Ok(());
        }
        for (name, value) in [
            ("max_header_bytes", self.max_header_bytes),
            ("max_headers", self.max_headers),
            ("max_concurrency", self.max_concurrency),
            ("max_response_bytes", self.max_response_bytes),
        ] {
            if value == 0 {
                return Err(format!("api_protection.{}.{} must be greater than zero", class.as_str(), name));
            }
        }
        if self.requests_per_minute == 0 || self.queue_timeout_ms == 0 || self.request_timeout_ms == 0 {
            return Err(format!("api_protection.{} rate and timeout values must be greater than zero", class.as_str()));
        }
        Ok(())
    }
}

impl Default for EndpointLimits {
    fn default() -> Self {
        Self {
            mode: ProtectionMode::Enforce,
            max_body_bytes: 1_048_576,
            max_header_bytes: 32_768,
            max_headers: 128,
            requests_per_minute: 600,
            max_concurrency: 16,
            queue_timeout_ms: 500,
            request_timeout_ms: 30_000,
            max_response_bytes: 2_097_152,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IngressProtectionConfig {
    pub global_max_concurrency: usize,
    pub max_rate_keys: usize,
    pub trusted_proxies: Vec<String>,
    /// Optional environment variable containing the bearer token required by every `http in`
    /// listener. The secret value never belongs in configuration.
    pub webhook_bearer_env: Option<String>,
    pub health: EndpointLimits,
    pub editor_admin: EndpointLimits,
    pub authentication: EndpointLimits,
    pub websocket: EndpointLimits,
    pub webhook: EndpointLimits,
    pub copilot: EndpointLimits,
    pub fleet: EndpointLimits,
    pub plugins: EndpointLimits,
    pub static_assets: EndpointLimits,
}

impl Default for IngressProtectionConfig {
    fn default() -> Self {
        let no_body = EndpointLimits { max_body_bytes: 0, ..EndpointLimits::default() };
        Self {
            global_max_concurrency: 128,
            max_rate_keys: 2_048,
            trusted_proxies: Vec::new(),
            webhook_bearer_env: None,
            health: EndpointLimits {
                requests_per_minute: 1_200,
                max_concurrency: 32,
                request_timeout_ms: 2_000,
                max_response_bytes: 65_536,
                ..no_body.clone()
            },
            editor_admin: EndpointLimits {
                max_body_bytes: 4_194_304,
                requests_per_minute: 600,
                max_concurrency: 32,
                request_timeout_ms: 120_000,
                max_response_bytes: 8_388_608,
                ..EndpointLimits::default()
            },
            authentication: EndpointLimits {
                max_body_bytes: 65_536,
                requests_per_minute: 60,
                max_concurrency: 8,
                request_timeout_ms: 15_000,
                max_response_bytes: 65_536,
                ..EndpointLimits::default()
            },
            websocket: EndpointLimits {
                max_body_bytes: 65_536,
                requests_per_minute: 120,
                max_concurrency: 32,
                request_timeout_ms: 10_000,
                max_response_bytes: 65_536,
                ..EndpointLimits::default()
            },
            webhook: EndpointLimits::default(),
            copilot: EndpointLimits {
                max_body_bytes: 1_048_576,
                requests_per_minute: 30,
                max_concurrency: 2,
                queue_timeout_ms: 1_000,
                request_timeout_ms: 70_000,
                max_response_bytes: 2_097_152,
                ..EndpointLimits::default()
            },
            fleet: EndpointLimits {
                requests_per_minute: 60,
                max_concurrency: 4,
                request_timeout_ms: 60_000,
                max_response_bytes: 4_194_304,
                ..EndpointLimits::default()
            },
            // DESIGN.md §10: one package (≤ max_module_kib) per request, rare and serialised.
            plugins: EndpointLimits {
                max_body_bytes: 1_048_576,
                requests_per_minute: 6,
                max_concurrency: 1,
                queue_timeout_ms: 500,
                request_timeout_ms: 30_000,
                max_response_bytes: 65_536,
                ..EndpointLimits::default()
            },
            static_assets: EndpointLimits {
                requests_per_minute: 1_200,
                max_concurrency: 64,
                max_response_bytes: 16_777_216,
                ..no_body
            },
        }
    }
}

impl IngressProtectionConfig {
    pub fn load(cfg: Option<&config::Config>) -> Result<Self, String> {
        let value = match cfg {
            Some(cfg) => match cfg.get::<Self>("api_protection") {
                Ok(value) => value,
                Err(config::ConfigError::NotFound(_)) => Self::default(),
                Err(err) => return Err(err.to_string()),
            },
            None => Self::default(),
        };
        value.validate()?;
        Ok(value)
    }

    pub fn limits(&self, class: EndpointClass) -> &EndpointLimits {
        match class {
            EndpointClass::Health => &self.health,
            EndpointClass::EditorAdmin => &self.editor_admin,
            EndpointClass::Authentication => &self.authentication,
            EndpointClass::Websocket => &self.websocket,
            EndpointClass::Webhook => &self.webhook,
            EndpointClass::Copilot => &self.copilot,
            EndpointClass::Fleet => &self.fleet,
            EndpointClass::Plugins => &self.plugins,
            EndpointClass::Static => &self.static_assets,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.global_max_concurrency == 0 || self.max_rate_keys == 0 {
            return Err("api_protection global limits must be greater than zero".to_string());
        }
        for class in [
            EndpointClass::Health,
            EndpointClass::EditorAdmin,
            EndpointClass::Authentication,
            EndpointClass::Websocket,
            EndpointClass::Webhook,
            EndpointClass::Copilot,
            EndpointClass::Fleet,
            EndpointClass::Plugins,
            EndpointClass::Static,
        ] {
            self.limits(class).validate(class)?;
        }
        TrustedProxySet::new(&self.trusted_proxies)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct TrustedProxySet(Vec<IpNetwork>);

impl TrustedProxySet {
    pub fn new(values: &[String]) -> Result<Self, String> {
        values
            .iter()
            .map(|value| IpNetwork::from_str(value).map_err(|err| format!("invalid trusted proxy '{value}': {err}")))
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }

    pub fn contains(&self, address: IpAddr) -> bool {
        self.0.iter().any(|network| network.contains(address))
    }
}

#[derive(Debug, Clone)]
struct IpNetwork {
    address: IpAddr,
    prefix: u8,
}

impl IpNetwork {
    fn contains(&self, candidate: IpAddr) -> bool {
        match (self.address, candidate) {
            (IpAddr::V4(network), IpAddr::V4(candidate)) => {
                let mask = if self.prefix == 0 { 0 } else { u32::MAX << (32 - self.prefix) };
                (u32::from(network) & mask) == (u32::from(candidate) & mask)
            }
            (IpAddr::V6(network), IpAddr::V6(candidate)) => {
                let mask = if self.prefix == 0 { 0 } else { u128::MAX << (128 - self.prefix) };
                (u128::from(network) & mask) == (u128::from(candidate) & mask)
            }
            _ => false,
        }
    }
}

impl FromStr for IpNetwork {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (address, prefix) = match value.split_once('/') {
            Some((address, prefix)) => {
                let address = address.parse::<IpAddr>().map_err(|err| err.to_string())?;
                let prefix = prefix.parse::<u8>().map_err(|err| err.to_string())?;
                (address, prefix)
            }
            None => {
                let address = value.parse::<IpAddr>().map_err(|err| err.to_string())?;
                let prefix = if address.is_ipv4() { 32 } else { 128 };
                (address, prefix)
            }
        };
        let maximum = if address.is_ipv4() { 32 } else { 128 };
        if prefix > maximum {
            return Err(format!("prefix {prefix} is too large"));
        }
        Ok(Self { address, prefix })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_bounded_and_valid() {
        let config = IngressProtectionConfig::default();
        config.validate().unwrap();
        assert_eq!(config.copilot.max_concurrency, 2);
        assert_eq!(config.plugins.max_concurrency, 1);
        assert_eq!(config.plugins.requests_per_minute, 6);
        assert_eq!(config.webhook.max_body_bytes, 1_048_576);
    }

    #[test]
    fn trusted_proxies_support_exact_addresses_and_cidr() {
        let proxies = TrustedProxySet::new(&["127.0.0.1".to_string(), "10.4.0.0/16".to_string()]).unwrap();
        assert!(proxies.contains("127.0.0.1".parse().unwrap()));
        assert!(proxies.contains("10.4.8.9".parse().unwrap()));
        assert!(!proxies.contains("10.5.8.9".parse().unwrap()));
    }

    #[test]
    fn invalid_or_zero_limits_fail_closed() {
        let cfg = config::Config::builder()
            .add_source(config::File::from_str(
                "[api_protection]\nglobal_max_concurrency = 0",
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap();
        assert!(IngressProtectionConfig::load(Some(&cfg)).unwrap_err().contains("greater than zero"));
        assert!(TrustedProxySet::new(&["10.0.0.0/99".to_string()]).is_err());
    }
}
