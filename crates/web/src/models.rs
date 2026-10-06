use std::collections::HashMap;

use config;
use serde::{Deserialize, Serialize};

/// Node-RED Flow data structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flow {
    pub id: String,
    pub label: Option<String>,
    pub nodes: Vec<FlowNode>,
    pub configs: Vec<FlowNode>,
    #[serde(rename = "type")]
    pub flow_type: String,
    pub disabled: Option<bool>,
    pub info: Option<String>,
    pub env: Option<Vec<EnvironmentVariable>>,
}

/// Node in a flow
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowNode {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: String,
    pub z: Option<String>, // flow id
    pub name: Option<String>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub wires: Vec<Vec<String>>,
    #[serde(flatten)]
    pub properties: HashMap<String, serde_json::Value>,
}

/// Environment variable
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentVariable {
    pub name: String,
    pub value: String,
    #[serde(rename = "type")]
    pub env_type: String,
}

/// Flows deployment/update payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowsPayload {
    pub flows: Vec<serde_json::Value>,
    pub rev: Option<String>,
}

/// Flow deployment response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowDeployResponse {
    pub rev: String,
}

/// Flow state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowState {
    pub state: String, // "start", "stop"
}

/// Node module information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeModule {
    pub name: String,
    pub version: String,
    pub nodes: Vec<NodeInfo>,
    pub enabled: bool,
    pub local: bool,
    pub user: Option<bool>,
}

/// Node information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub node_type: String,
    pub enabled: bool,
    pub config: Option<String>,
    pub help: Option<String>,
    pub version: Option<String>,
    pub local: Option<bool>,
    pub module: Option<String>,
}

/*
{
    "httpNodeRoot": "/",
    "version": "5.0.7",
    "context": {
        "default": "memory0",
        "stores": [
            "memory0",
            "memory1",
            "file"
        ]
    },
    "codeEditor": {
        "lib": "monaco",
        "options": {}
    },
    "libraries": [
        {
            "id": "local",
            "label": "editor:library.types.local",
            "user": false,
            "icon": "font-awesome/fa-hdd-o"
        },
        {
            "id": "examples",
            "label": "editor:library.types.examples",
            "user": false,
            "icon": "font-awesome/fa-life-ring",
            "types": [
                "flows"
            ],
            "readOnly": true
        }
    ],
    "flowFilePretty": true,
    "externalModules": {},
    "flowEncryptionType": "system",
    "diagnostics": {
        "enabled": true,
        "ui": true
    },
    "runtimeState": {
        "enabled": false,
        "ui": false
    },
    "functionExternalModules": true,
    "functionTimeout": 0,
    "tlsConfigDisableLocalFiles": false,
    "editorTheme": {
        "palette": {},
        "projects": {
            "enabled": false,
            "workflow": {
                "mode": "manual"
            }
        },
        "languages": [
            "de",
            "en-US",
            "es-ES",
            "fr",
            "ja",
            "ko",
            "pt-BR",
            "ru",
            "zh-CN",
            "zh-TW"
        ]
    }
}
    */

/// Library entry for UI settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryEntry {
    pub id: String,
    pub label: String,
    pub user: Option<bool>,
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub types: Option<Vec<String>>,
    #[serde(default, rename = "readOnly", skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
}

/// System settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedSystemSettings {
    pub version: String,
    #[serde(rename = "httpNodeRoot")]
    pub http_node_root: String,
    #[serde(rename = "httpAdminRoot")]
    pub http_admin_root: String,
    #[serde(rename = "httpStatic")]
    pub http_static: Option<String>,
    #[serde(rename = "uiHost")]
    pub ui_host: String,
    #[serde(rename = "uiPort")]
    pub ui_port: u16,
    #[serde(rename = "editorTheme")]
    pub editor_theme: EditorTheme,
    pub context: ContextConfig,
    pub logging: LoggingConfig,
    #[serde(default = "default_libraries")]
    pub libraries: Vec<LibraryEntry>,
    #[serde(default, rename = "configEditor")]
    pub config_editor: bool,
    /// Present only when an admin password, user list, or OIDC issuer is configured.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "adminAuth")]
    pub admin_auth: Option<AdminAuthSettings>,
    /// Present only for a signed-in request. Omitted when admin auth is off so the editor
    /// keeps the deploy button.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<SettingsUser>,
}

/// Tells the editor which login form to show. Passwords are not part of this object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminAuthSettings {
    #[serde(rename = "type")]
    pub auth_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsUser {
    pub username: String,
    pub permissions: String,
    #[serde(default)]
    pub anonymous: bool,
}

/// Editor theme configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditorTheme {
    pub page: ThemePage,
    pub header: ThemeHeader,
    #[serde(rename = "deployButton")]
    pub deploy_button: ThemeDeployButton,
    pub menu: ThemeMenu,
    #[serde(rename = "userMenu")]
    pub user_menu: bool,
    pub login: ThemeLogin,
    /// Locale codes the editor can select. Node-RED sends an array here;
    /// the settings tray calls `.map` on it and throws if this is an object.
    pub languages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemePage {
    pub title: String,
    pub favicon: Option<String>,
    pub css: Option<String>,
    pub scripts: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeHeader {
    pub title: String,
    pub url: Option<String>,
    pub image: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeDeployButton {
    #[serde(rename = "type")]
    pub button_type: String,
    pub label: Option<String>,
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeMenu {
    #[serde(rename = "menu-item-import-library")]
    pub menu_item_import_library: bool,
    #[serde(rename = "menu-item-export-library")]
    pub menu_item_export_library: bool,
    #[serde(rename = "menu-item-keyboard-shortcuts")]
    pub menu_item_keyboard_shortcuts: bool,
    #[serde(rename = "menu-item-help")]
    pub menu_item_help: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeLogin {
    pub image: Option<String>,
}

/// Context configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    pub default: String,
    pub stores: Vec<String>,
}

/// Logging configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub console: ConsoleLogging,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleLogging {
    pub level: String,
    pub metrics: bool,
    pub audit: bool,
}

/// API error response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

/// API success response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse<T> {
    pub data: T,
}

fn default_libraries() -> Vec<LibraryEntry> {
    vec![
        LibraryEntry {
            id: "local".to_string(),
            label: "editor:library.types.local".to_string(),
            user: Some(false),
            icon: Some("font-awesome/fa-hdd-o".to_string()),
            types: None,
            read_only: None,
        },
        LibraryEntry {
            id: "examples".to_string(),
            label: "editor:library.types.examples".to_string(),
            user: Some(false),
            icon: Some("font-awesome/fa-life-ring".to_string()),
            types: Some(vec!["flows".to_string()]),
            read_only: Some(true),
        },
    ]
}

impl Default for RedSystemSettings {
    fn default() -> Self {
        Self {
            version: "5.0.7".to_string(),
            http_node_root: "/".to_string(),
            http_admin_root: "/".to_string(),
            http_static: None,
            ui_host: "0.0.0.0".to_string(),
            ui_port: 1880,
            editor_theme: EditorTheme::default(),
            context: ContextConfig::default(),
            logging: LoggingConfig::default(),
            libraries: default_libraries(),
            config_editor: false,
            admin_auth: None,
            user: None,
        }
    }
}

impl Default for EditorTheme {
    fn default() -> Self {
        Self {
            page: ThemePage { title: "EdgeLinkd".to_string(), favicon: None, css: None, scripts: None },
            header: ThemeHeader { title: "EdgeLinkd".to_string(), url: None, image: None },
            deploy_button: ThemeDeployButton { button_type: "simple".to_string(), label: None, icon: None },
            menu: ThemeMenu {
                menu_item_import_library: true,
                menu_item_export_library: true,
                menu_item_keyboard_shortcuts: true,
                menu_item_help: HashMap::new(),
            },
            user_menu: false,
            login: ThemeLogin { image: None },
            languages: ["de", "en-US", "es-ES", "fr", "ja", "ko", "pt-BR", "ru", "zh-CN", "zh-TW"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        }
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self { default: "default".to_string(), stores: vec!["default".to_string()] }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self { console: ConsoleLogging { level: "info".to_string(), metrics: false, audit: false } }
    }
}

// Load WebServerArgs from config, fallback to default if not found
impl RedSystemSettings {
    pub fn load(cfg: &config::Config) -> n2link_core::Result<Self> {
        match cfg.get::<Self>("web") {
            Ok(res) => Ok(res),
            Err(config::ConfigError::NotFound(_)) => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_languages_serialize_as_locale_codes() {
        let json = serde_json::to_value(RedSystemSettings::default()).expect("settings json");
        let languages = json["editorTheme"]["languages"].as_array().expect("languages array");
        assert!(languages.iter().all(|code| code.as_str().is_some()));
        assert!(languages.iter().any(|code| code == "en-US"));
    }
}
