//! Shared outbound-network policy.
//!
//! `off` preserves the historical behavior. `observe` records decisions but permits them, and
//! `enforce` requires every resolved address to be covered by an administrator rule. Callers must
//! use the returned addresses for the actual connection; resolving again would reopen a DNS
//! rebinding window.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::net::{TcpStream, lookup_host};
use url::{Host, Url};

use crate::{N2linkError, Result};

const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_IDLE_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 1_048_576;
const DEFAULT_MAX_REDIRECTS: usize = 5;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum EgressMode {
    #[default]
    Off,
    Observe,
    Enforce,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, Hash, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkProtocol {
    Http,
    Https,
    Mqtt,
    Mqtts,
    Ws,
    Wss,
    Tcp,
    Udp,
    Modbus,
}

impl fmt::Display for NetworkProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Http => "http",
                Self::Https => "https",
                Self::Mqtt => "mqtt",
                Self::Mqtts => "mqtts",
                Self::Ws => "ws",
                Self::Wss => "wss",
                Self::Tcp => "tcp",
                Self::Udp => "udp",
                Self::Modbus => "modbus",
            }
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub enum EgressPurpose {
    HttpNode,
    AiProvider,
    Oidc,
    Fleet,
    Proxy,
    Mqtt,
    WebSocket,
    Tcp,
    Udp,
    Modbus,
    Postgres,
    Redis,
}

impl fmt::Display for EgressPurpose {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::HttpNode => "http-node",
                Self::AiProvider => "ai-provider",
                Self::Oidc => "oidc",
                Self::Fleet => "fleet",
                Self::Proxy => "proxy",
                Self::Mqtt => "mqtt",
                Self::WebSocket => "websocket",
                Self::Tcp => "tcp",
                Self::Udp => "udp",
                Self::Modbus => "modbus",
                Self::Postgres => "postgres",
                Self::Redis => "redis",
            }
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct EgressConfig {
    pub mode: EgressMode,
    pub allow_environment_proxy: bool,
    pub proxy_url: Option<String>,
    pub connect_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub max_response_bytes: usize,
    pub max_redirects: usize,
    pub allow: Vec<EgressRule>,
}

impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            mode: EgressMode::Off,
            allow_environment_proxy: false,
            proxy_url: None,
            connect_timeout_ms: DEFAULT_CONNECT_TIMEOUT_MS,
            request_timeout_ms: DEFAULT_REQUEST_TIMEOUT_MS,
            idle_timeout_ms: DEFAULT_IDLE_TIMEOUT_MS,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            allow: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EgressRule {
    pub protocols: Vec<NetworkProtocol>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub cidr: Option<String>,
    pub ports: Vec<u16>,
}

#[derive(Clone, Copy, Debug)]
struct Cidr {
    address: IpAddr,
    prefix: u8,
}

impl Cidr {
    fn parse(value: &str) -> Result<Self> {
        let (address, prefix) =
            value.split_once('/').ok_or_else(|| config_error("egress allow CIDR must contain a prefix"))?;
        let address = IpAddr::from_str(address).map_err(|_| config_error("egress allow CIDR address is invalid"))?;
        let prefix = prefix.parse::<u8>().map_err(|_| config_error("egress allow CIDR prefix is invalid"))?;
        let max = if address.is_ipv4() { 32 } else { 128 };
        if prefix > max {
            return Err(config_error("egress allow CIDR prefix is out of range"));
        }
        Ok(Self { address, prefix })
    }

    fn contains(self, candidate: IpAddr) -> bool {
        match (self.address, candidate) {
            (IpAddr::V4(network), IpAddr::V4(candidate)) => {
                let mask = if self.prefix == 0 { 0 } else { u32::MAX << (32 - self.prefix) };
                u32::from(network) & mask == u32::from(candidate) & mask
            }
            (IpAddr::V6(network), IpAddr::V6(candidate)) => {
                let mask = if self.prefix == 0 { 0 } else { u128::MAX << (128 - self.prefix) };
                u128::from(network) & mask == u128::from(candidate) & mask
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug)]
struct Rule {
    protocols: Vec<NetworkProtocol>,
    host: Option<String>,
    cidr: Option<Cidr>,
    ports: Vec<u16>,
}

impl TryFrom<EgressRule> for Rule {
    type Error = N2linkError;

    fn try_from(raw: EgressRule) -> Result<Self> {
        if raw.protocols.is_empty() || raw.ports.is_empty() {
            return Err(config_error("egress allow rules require protocols and ports"));
        }
        if raw.ports.contains(&0) {
            return Err(config_error("egress allow rule port 0 is invalid"));
        }
        let host = raw.host.map(|host| normalize_rule_host(&host)).transpose()?;
        let cidr = raw.cidr.as_deref().map(Cidr::parse).transpose()?;
        if host.is_none() && cidr.is_none() {
            return Err(config_error("egress allow rules require host or cidr"));
        }
        Ok(Self { protocols: raw.protocols, host, cidr, ports: raw.ports })
    }
}

fn normalize_rule_host(value: &str) -> Result<String> {
    let host = value.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host.contains('*') || host.contains('@') || host.contains('/') || host.contains(':') {
        // IPv6 literals belong in CIDR so brackets/colon parser differentials cannot slip in.
        return Err(config_error("egress allow host must be an exact hostname or IPv4 literal"));
    }
    Ok(host)
}

fn validate_proxy_url(value: &str) -> Result<String> {
    let url = Url::parse(value).map_err(|_| config_error("egress proxy_url is invalid"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || (url.path() != "/" && !url.path().is_empty())
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(config_error(
            "egress proxy_url must be an HTTP(S) origin without credentials, path, query, or fragment",
        ));
    }
    Ok(url.to_string())
}

#[derive(Clone, Debug)]
pub struct EgressPolicy {
    config: EgressConfig,
    mode: EgressMode,
    allow_environment_proxy: bool,
    // Read only by the HTTP client, which builds with `nodes_http` or `nodes_ai`.
    #[cfg_attr(not(any(feature = "nodes_http", feature = "nodes_ai")), allow(dead_code))]
    proxy_url: Option<String>,
    connect_timeout: Duration,
    request_timeout: Duration,
    idle_timeout: Duration,
    max_response_bytes: usize,
    max_redirects: usize,
    rules: Vec<Rule>,
}

impl Default for EgressPolicy {
    fn default() -> Self {
        Self::from_config(EgressConfig::default()).expect("the built-in egress policy is valid")
    }
}

impl EgressPolicy {
    pub fn load(cfg: Option<&config::Config>) -> Result<Arc<Self>> {
        let raw = match cfg {
            Some(cfg) => match cfg.get::<EgressConfig>("egress") {
                Ok(raw) => raw,
                Err(config::ConfigError::NotFound(_)) => EgressConfig::default(),
                Err(err) => return Err(N2linkError::Other(anyhow::Error::new(err).context("invalid egress policy"))),
            },
            None => EgressConfig::default(),
        };
        Ok(Arc::new(Self::from_config(raw)?))
    }

    pub fn from_config(raw: EgressConfig) -> Result<Self> {
        if raw.connect_timeout_ms == 0 || raw.request_timeout_ms == 0 || raw.idle_timeout_ms == 0 {
            return Err(config_error("egress timeouts must be greater than zero"));
        }
        if raw.max_response_bytes == 0 {
            return Err(config_error("egress max_response_bytes must be greater than zero"));
        }
        if raw.mode != EgressMode::Off && raw.allow_environment_proxy {
            return Err(config_error("ambient environment proxies cannot be pinned; configure proxy_url explicitly"));
        }
        let proxy_url = raw.proxy_url.as_deref().map(validate_proxy_url).transpose()?;
        let rules = raw.allow.iter().cloned().map(Rule::try_from).collect::<Result<Vec<_>>>()?;
        Ok(Self {
            config: raw.clone(),
            mode: raw.mode,
            allow_environment_proxy: raw.allow_environment_proxy,
            proxy_url,
            connect_timeout: Duration::from_millis(raw.connect_timeout_ms),
            request_timeout: Duration::from_millis(raw.request_timeout_ms),
            idle_timeout: Duration::from_millis(raw.idle_timeout_ms),
            max_response_bytes: raw.max_response_bytes,
            max_redirects: raw.max_redirects,
            rules,
        })
    }

    pub fn config(&self) -> &EgressConfig {
        &self.config
    }

    pub fn mode(&self) -> EgressMode {
        self.mode
    }

    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    pub fn max_response_bytes(&self) -> usize {
        self.max_response_bytes
    }

    pub fn max_redirects(&self) -> usize {
        self.max_redirects
    }

    pub fn allow_environment_proxy(&self) -> bool {
        self.allow_environment_proxy
    }

    pub async fn approve_url(&self, purpose: EgressPurpose, value: &str) -> Result<ApprovedTarget> {
        let url = Url::parse(value).map_err(|_| denied("outbound URL is invalid"))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(denied("outbound URL user-info is not allowed"));
        }
        let protocol = match url.scheme() {
            "http" => NetworkProtocol::Http,
            "https" => NetworkProtocol::Https,
            "ws" => NetworkProtocol::Ws,
            "wss" => NetworkProtocol::Wss,
            _ => return Err(denied("outbound URL scheme is not supported")),
        };
        let host = match url.host() {
            Some(Host::Domain(host)) => host.trim_end_matches('.').to_ascii_lowercase(),
            Some(Host::Ipv4(ip)) => ip.to_string(),
            Some(Host::Ipv6(ip)) => ip.to_string(),
            None => return Err(denied("outbound URL has no host")),
        };
        let port = url.port_or_known_default().ok_or_else(|| denied("outbound URL has no port"))?;
        self.approve(purpose, protocol, &host, port).await
    }

    pub async fn approve(
        &self,
        purpose: EgressPurpose,
        protocol: NetworkProtocol,
        host: &str,
        port: u16,
    ) -> Result<ApprovedTarget> {
        if self.mode == EgressMode::Off {
            return Ok(ApprovedTarget { host: host.to_owned(), port, addresses: Vec::new() });
        }
        let host = normalize_target_host(host)?;
        let addresses = if let Ok(ip) = IpAddr::from_str(&host) {
            vec![ip]
        } else {
            let mut values = lookup_host((host.as_str(), port))
                .await
                .map_err(|_| denied("outbound hostname resolution failed"))?
                .map(|value| value.ip())
                .collect::<Vec<_>>();
            values.sort();
            values.dedup();
            if values.is_empty() {
                return Err(denied("outbound hostname resolved to no addresses"));
            }
            values
        };
        self.check_resolved(purpose, protocol, &host, port, &addresses)?;
        Ok(ApprovedTarget { host, port, addresses })
    }

    fn check_resolved(
        &self,
        purpose: EgressPurpose,
        protocol: NetworkProtocol,
        host: &str,
        port: u16,
        addresses: &[IpAddr],
    ) -> Result<()> {
        let allowed = !addresses.is_empty()
            && !addresses.iter().any(|address| is_metadata(*address))
            && addresses
                .iter()
                .all(|address| self.rules.iter().any(|rule| rule_allows(rule, protocol, host, port, *address)));
        let reason = if allowed { "allowed" } else { "not-allowlisted" };
        match self.mode {
            EgressMode::Off => Ok(()),
            EgressMode::Observe => {
                log::info!(
                    "egress decision mode=observe action=permit purpose={purpose} protocol={protocol} port={port} reason={reason}"
                );
                Ok(())
            }
            EgressMode::Enforce if allowed => {
                log::info!(
                    "egress decision mode=enforce action=permit purpose={purpose} protocol={protocol} port={port} reason=allowed"
                );
                Ok(())
            }
            EgressMode::Enforce => {
                log::warn!(
                    "egress decision mode=enforce action=deny purpose={purpose} protocol={protocol} port={port} reason={reason}"
                );
                Err(denied("outbound target is not allowed by egress policy"))
            }
        }
    }

    pub async fn connect_tcp(
        &self,
        purpose: EgressPurpose,
        protocol: NetworkProtocol,
        host: &str,
        port: u16,
    ) -> Result<TcpStream> {
        let approved = self.approve(purpose, protocol, host, port).await?;
        if approved.addresses.is_empty() {
            return tokio::time::timeout(self.connect_timeout, TcpStream::connect((approved.host.as_str(), port)))
                .await
                .map_err(|_| N2linkError::Timeout)?
                .map_err(N2linkError::from);
        }
        let mut last = None;
        for address in approved.socket_addrs() {
            match tokio::time::timeout(self.connect_timeout, TcpStream::connect(address)).await {
                Ok(Ok(stream)) => return Ok(stream),
                Ok(Err(err)) => last = Some(N2linkError::from(err)),
                Err(_) => last = Some(N2linkError::Timeout),
            }
        }
        Err(last.unwrap_or_else(|| denied("outbound target has no approved address")))
    }

    #[cfg(feature = "nodes_websocket")]
    pub async fn connect_websocket(
        &self,
        value: &str,
    ) -> Result<(
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    )> {
        if self.mode == EgressMode::Off {
            return tokio_tungstenite::connect_async(value)
                .await
                .map_err(|_| denied("outbound WebSocket connection failed"));
        }
        let url = Url::parse(value).map_err(|_| denied("outbound WebSocket URL is invalid"))?;
        let protocol = match url.scheme() {
            "ws" => NetworkProtocol::Ws,
            "wss" => NetworkProtocol::Wss,
            _ => return Err(denied("outbound WebSocket scheme is not supported")),
        };
        let host = url.host_str().ok_or_else(|| denied("outbound WebSocket URL has no host"))?;
        let port = url.port_or_known_default().ok_or_else(|| denied("outbound WebSocket URL has no port"))?;
        let stream = self.connect_tcp(EgressPurpose::WebSocket, protocol, host, port).await?;
        tokio_tungstenite::client_async_tls_with_config(value, stream, None, None)
            .await
            .map_err(|_| denied("outbound WebSocket handshake failed"))
    }

    #[cfg(any(feature = "nodes_http", feature = "nodes_ai"))]
    pub async fn http_client(&self, purpose: EgressPurpose, url: &str) -> Result<reqwest::Client> {
        if self.mode == EgressMode::Off {
            return reqwest::Client::builder()
                .connect_timeout(self.connect_timeout)
                .timeout(self.request_timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| denied("outbound HTTP client could not be created"));
        }
        let approved = self.approve_url(purpose, url).await?;
        let mut builder = reqwest::Client::builder()
            .connect_timeout(self.connect_timeout)
            .timeout(self.request_timeout)
            .redirect(reqwest::redirect::Policy::none());
        builder = builder.no_proxy();
        if !approved.addresses.is_empty() {
            builder = builder.resolve_to_addrs(&approved.host, &approved.socket_addrs());
        }
        if let Some(proxy_url) = &self.proxy_url {
            let proxy = self.approve_url(EgressPurpose::Proxy, proxy_url).await?;
            if !proxy.addresses.is_empty() {
                builder = builder.resolve_to_addrs(&proxy.host, &proxy.socket_addrs());
            }
            builder =
                builder.proxy(reqwest::Proxy::all(proxy_url).map_err(|_| config_error("egress proxy_url is invalid"))?);
        }
        builder.build().map_err(|_| denied("outbound HTTP client could not be created"))
    }
}

/// A process-wide policy reference that can be replaced after a validated configuration save.
/// Callers clone a snapshot before awaiting, so no synchronization guard spans network I/O.
#[derive(Clone, Debug)]
pub struct EgressPolicyHandle {
    inner: Arc<std::sync::RwLock<Arc<EgressPolicy>>>,
}

impl Default for EgressPolicyHandle {
    fn default() -> Self {
        Self::new(EgressPolicy::default())
    }
}

impl EgressPolicyHandle {
    pub fn new(policy: EgressPolicy) -> Self {
        Self { inner: Arc::new(std::sync::RwLock::new(Arc::new(policy))) }
    }

    pub fn load(cfg: Option<&config::Config>) -> Result<Self> {
        let policy = EgressPolicy::load(cfg)?;
        Ok(Self { inner: Arc::new(std::sync::RwLock::new(policy)) })
    }

    pub fn snapshot(&self) -> Arc<EgressPolicy> {
        self.inner.read().unwrap_or_else(|err| err.into_inner()).clone()
    }

    pub fn replace(&self, policy: EgressPolicy) -> Arc<EgressPolicy> {
        let mut guard = self.inner.write().unwrap_or_else(|err| err.into_inner());
        std::mem::replace(&mut *guard, Arc::new(policy))
    }

    pub fn replace_arc(&self, policy: Arc<EgressPolicy>) -> Arc<EgressPolicy> {
        let mut guard = self.inner.write().unwrap_or_else(|err| err.into_inner());
        std::mem::replace(&mut *guard, policy)
    }

    pub fn mode(&self) -> EgressMode {
        self.snapshot().mode()
    }

    pub fn connect_timeout(&self) -> Duration {
        self.snapshot().connect_timeout()
    }

    pub fn request_timeout(&self) -> Duration {
        self.snapshot().request_timeout()
    }

    pub fn idle_timeout(&self) -> Duration {
        self.snapshot().idle_timeout()
    }

    pub fn max_response_bytes(&self) -> usize {
        self.snapshot().max_response_bytes()
    }

    pub fn max_redirects(&self) -> usize {
        self.snapshot().max_redirects()
    }

    pub fn config(&self) -> EgressConfig {
        self.snapshot().config().clone()
    }

    pub async fn approve(
        &self,
        purpose: EgressPurpose,
        protocol: NetworkProtocol,
        host: &str,
        port: u16,
    ) -> Result<ApprovedTarget> {
        self.snapshot().approve(purpose, protocol, host, port).await
    }

    pub async fn connect_tcp(
        &self,
        purpose: EgressPurpose,
        protocol: NetworkProtocol,
        host: &str,
        port: u16,
    ) -> Result<TcpStream> {
        self.snapshot().connect_tcp(purpose, protocol, host, port).await
    }

    #[cfg(feature = "nodes_websocket")]
    pub async fn connect_websocket(
        &self,
        value: &str,
    ) -> Result<(
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    )> {
        self.snapshot().connect_websocket(value).await
    }

    #[cfg(any(feature = "nodes_http", feature = "nodes_ai"))]
    pub async fn http_client(&self, purpose: EgressPurpose, url: &str) -> Result<reqwest::Client> {
        self.snapshot().http_client(purpose, url).await
    }
}

#[derive(Clone, Debug)]
pub struct ApprovedTarget {
    pub host: String,
    pub port: u16,
    pub addresses: Vec<IpAddr>,
}

impl ApprovedTarget {
    pub fn socket_addrs(&self) -> Vec<SocketAddr> {
        self.addresses.iter().map(|address| SocketAddr::new(*address, self.port)).collect()
    }
}

fn rule_allows(rule: &Rule, protocol: NetworkProtocol, host: &str, port: u16, address: IpAddr) -> bool {
    if !rule.protocols.contains(&protocol) || !rule.ports.contains(&port) {
        return false;
    }
    if let Some(expected) = &rule.host
        && expected != host
    {
        return false;
    }
    if let Some(cidr) = rule.cidr
        && !cidr.contains(address)
    {
        return false;
    }
    // A DNS name resolving to a non-public address needs an address-bound rule. An exact IP
    // literal is itself address-bound and remains convenient for industrial configurations.
    !is_unsafe(address) || rule.cidr.is_some() || IpAddr::from_str(host) == Ok(address)
}

fn normalize_target_host(value: &str) -> Result<String> {
    let host = value.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host.contains('@') || host.contains('/') || host.contains('\0') {
        return Err(denied("outbound host is invalid"));
    }
    if metadata_hostname(&host) {
        // Still allow an explicit IP/CIDR rule to decide this in check_resolved, but never ask DNS
        // for well-known metadata aliases.
        return Err(denied("cloud metadata hostname is not allowed"));
    }
    Ok(host.trim_start_matches('[').trim_end_matches(']').to_owned())
}

fn metadata_hostname(host: &str) -> bool {
    matches!(host, "metadata.google.internal" | "metadata" | "instance-data.ec2.internal")
}

fn is_unsafe(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => is_unsafe_v4(ip),
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (ip.segments()[0] & 0xfe00) == 0xfc00
                || (ip.segments()[0] & 0xffc0) == 0xfe80
                || ip.to_ipv4_mapped().is_some_and(is_unsafe_v4)
                || ip == Ipv6Addr::from_str("fd00:ec2::254").expect("constant IPv6 address")
        }
    }
}

fn is_metadata(address: IpAddr) -> bool {
    address == IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))
        || address == IpAddr::V6(Ipv6Addr::from_str("fd00:ec2::254").expect("constant IPv6 address"))
}

fn is_unsafe_v4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_unspecified()
        || octets[0] == 0
        || octets[0] >= 240
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || ip == Ipv4Addr::new(169, 254, 169, 254)
}

fn config_error(message: &str) -> N2linkError {
    N2linkError::InvalidOperation(format!("invalid egress configuration: {message}"))
}

fn denied(message: &'static str) -> N2linkError {
    N2linkError::InvalidOperation(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: EgressMode, rules: Vec<EgressRule>) -> EgressPolicy {
        EgressPolicy::from_config(EgressConfig { mode, allow: rules, ..EgressConfig::default() }).unwrap()
    }

    fn rule(protocol: NetworkProtocol, host: Option<&str>, cidr: Option<&str>, port: u16) -> EgressRule {
        EgressRule {
            protocols: vec![protocol],
            host: host.map(str::to_owned),
            cidr: cidr.map(str::to_owned),
            ports: vec![port],
        }
    }

    #[test]
    fn cidr_matches_ipv4_and_ipv6() {
        assert!(Cidr::parse("10.2.0.0/16").unwrap().contains("10.2.3.4".parse().unwrap()));
        assert!(!Cidr::parse("10.2.0.0/16").unwrap().contains("10.3.3.4".parse().unwrap()));
        assert!(Cidr::parse("fd00::/8").unwrap().contains("fd12::1".parse().unwrap()));
    }

    #[test]
    fn invalid_rules_fail_closed() {
        for raw in [
            rule(NetworkProtocol::Mqtt, Some("*.local"), None, 1883),
            rule(NetworkProtocol::Mqtt, None, None, 1883),
            rule(NetworkProtocol::Mqtt, Some("broker"), None, 0),
            rule(NetworkProtocol::Mqtt, None, Some("10.0.0.0/33"), 1883),
        ] {
            assert!(Rule::try_from(raw).is_err());
        }
    }

    #[test]
    fn private_dns_requires_an_address_rule() {
        let host_only =
            policy(EgressMode::Enforce, vec![rule(NetworkProtocol::Mqtt, Some("broker.local"), None, 1883)]);
        assert!(
            host_only
                .check_resolved(
                    EgressPurpose::Mqtt,
                    NetworkProtocol::Mqtt,
                    "broker.local",
                    1883,
                    &["192.168.1.2".parse().unwrap()],
                )
                .is_err()
        );
        let bound = policy(
            EgressMode::Enforce,
            vec![rule(NetworkProtocol::Mqtt, Some("broker.local"), Some("192.168.1.0/24"), 1883)],
        );
        assert!(
            bound
                .check_resolved(
                    EgressPurpose::Mqtt,
                    NetworkProtocol::Mqtt,
                    "broker.local",
                    1883,
                    &["192.168.1.2".parse().unwrap()],
                )
                .is_ok()
        );
    }

    #[test]
    fn every_dns_answer_must_be_allowed() {
        let policy = policy(
            EgressMode::Enforce,
            vec![rule(NetworkProtocol::Https, Some("api.example"), Some("203.0.113.0/24"), 443)],
        );
        assert!(
            policy
                .check_resolved(
                    EgressPurpose::AiProvider,
                    NetworkProtocol::Https,
                    "api.example",
                    443,
                    &["203.0.113.7".parse().unwrap(), "127.0.0.1".parse().unwrap()],
                )
                .is_err()
        );
    }

    #[test]
    fn protocol_and_port_are_part_of_the_decision() {
        let policy = policy(EgressMode::Enforce, vec![rule(NetworkProtocol::Mqtt, Some("127.0.0.1"), None, 1883)]);
        assert!(
            policy
                .check_resolved(
                    EgressPurpose::Mqtt,
                    NetworkProtocol::Mqtt,
                    "127.0.0.1",
                    1883,
                    &["127.0.0.1".parse().unwrap()],
                )
                .is_ok()
        );
        assert!(
            policy
                .check_resolved(
                    EgressPurpose::Mqtt,
                    NetworkProtocol::Tcp,
                    "127.0.0.1",
                    1883,
                    &["127.0.0.1".parse().unwrap()],
                )
                .is_err()
        );
    }

    #[tokio::test]
    async fn user_info_and_metadata_aliases_are_rejected() {
        let policy = policy(EgressMode::Enforce, Vec::new());
        assert!(policy.approve_url(EgressPurpose::HttpNode, "http://user:token@example.com/").await.is_err());
        assert!(policy.approve_url(EgressPurpose::HttpNode, "http://metadata.google.internal/").await.is_err());
    }

    #[test]
    fn observe_records_but_does_not_block() {
        let policy = policy(EgressMode::Observe, Vec::new());
        assert!(
            policy
                .check_resolved(
                    EgressPurpose::Tcp,
                    NetworkProtocol::Tcp,
                    "127.0.0.1",
                    9,
                    &["127.0.0.1".parse().unwrap()],
                )
                .is_ok()
        );
    }

    #[test]
    fn unsafe_address_classes_are_classified() {
        for value in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "224.0.0.1",
            "::",
            "::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
        ] {
            assert!(is_unsafe(value.parse().unwrap()), "{value}");
        }
        assert!(!is_unsafe("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn metadata_addresses_are_never_allowlisted() {
        let policy = policy(EgressMode::Enforce, vec![rule(NetworkProtocol::Http, None, Some("169.254.0.0/16"), 80)]);
        assert!(
            policy
                .check_resolved(
                    EgressPurpose::HttpNode,
                    NetworkProtocol::Http,
                    "169.254.169.254",
                    80,
                    &["169.254.169.254".parse().unwrap()],
                )
                .is_err()
        );
    }

    #[test]
    fn configuration_parses_limits_and_local_mqtt_rule() {
        let cfg = config::Config::builder()
            .add_source(config::File::from_str(
                r#"
                [egress]
                mode = "enforce"
                connect_timeout_ms = 123
                request_timeout_ms = 456
                idle_timeout_ms = 78
                max_response_bytes = 99
                max_redirects = 2

                [[egress.allow]]
                protocols = ["mqtt"]
                host = "127.0.0.1"
                ports = [1883]
                "#,
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap();
        let policy = EgressPolicy::load(Some(&cfg)).unwrap();
        assert_eq!(policy.mode(), EgressMode::Enforce);
        assert_eq!(policy.connect_timeout(), Duration::from_millis(123));
        assert_eq!(policy.request_timeout(), Duration::from_millis(456));
        assert_eq!(policy.idle_timeout(), Duration::from_millis(78));
        assert_eq!(policy.max_response_bytes(), 99);
        assert_eq!(policy.max_redirects(), 2);
        assert!(!policy.allow_environment_proxy());
    }

    #[test]
    fn invalid_configuration_is_rejected() {
        for text in [
            "[egress]\nmode = \"maybe\"",
            "[egress]\nconnect_timeout_ms = 0",
            "[egress]\nmax_response_bytes = 0",
            "[egress]\nunknown = true",
            "[egress]\nmode = \"observe\"\nallow_environment_proxy = true",
            "[egress]\nmode = \"enforce\"\nproxy_url = \"http://user:secret@127.0.0.1:3128\"",
        ] {
            let cfg = config::Config::builder()
                .add_source(config::File::from_str(text, config::FileFormat::Toml))
                .build()
                .unwrap();
            assert!(EgressPolicy::load(Some(&cfg)).is_err(), "{text}");
        }
    }

    #[test]
    fn a_shared_handle_switches_every_clone_atomically() {
        let handle = EgressPolicyHandle::default();
        let second = handle.clone();
        assert_eq!(handle.mode(), EgressMode::Off);

        let old = handle.replace(policy(EgressMode::Enforce, vec![]));

        assert_eq!(old.mode(), EgressMode::Off);
        assert_eq!(handle.mode(), EgressMode::Enforce);
        assert_eq!(second.mode(), EgressMode::Enforce);
    }

    #[tokio::test]
    async fn enforce_denies_then_observe_and_an_explicit_rule_connect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let denied_policy = policy(EgressMode::Enforce, Vec::new());
        assert!(denied_policy.connect_tcp(EgressPurpose::Tcp, NetworkProtocol::Tcp, "127.0.0.1", port).await.is_err());

        let observe = policy(EgressMode::Observe, Vec::new());
        let observed = observe.connect_tcp(EgressPurpose::Tcp, NetworkProtocol::Tcp, "127.0.0.1", port);
        let (connected, accepted) = tokio::join!(observed, listener.accept());
        assert!(connected.is_ok());
        assert!(accepted.is_ok());

        let enforce = policy(EgressMode::Enforce, vec![rule(NetworkProtocol::Tcp, Some("127.0.0.1"), None, port)]);
        let allowed = enforce.connect_tcp(EgressPurpose::Tcp, NetworkProtocol::Tcp, "127.0.0.1", port);
        let (connected, accepted) = tokio::join!(allowed, listener.accept());
        assert!(connected.is_ok());
        assert!(accepted.is_ok());
    }
}
