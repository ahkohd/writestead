use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SyncBackend {
    Obsidian,
    None,
}

impl Display for SyncBackend {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncBackend::Obsidian => write!(f, "obsidian"),
            SyncBackend::None => write!(f, "none"),
        }
    }
}

impl std::str::FromStr for SyncBackend {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_lowercase().as_str() {
            "obsidian" => Ok(SyncBackend::Obsidian),
            "none" => Ok(SyncBackend::None),
            other => Err(anyhow!(
                "invalid sync backend '{}', expected 'obsidian' or 'none'",
                other
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    pub backend: SyncBackend,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            backend: SyncBackend::Obsidian,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum McpAuthMode {
    #[default]
    None,
    Bearer,
}

impl Display for McpAuthMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            McpAuthMode::None => write!(f, "none"),
            McpAuthMode::Bearer => write!(f, "bearer"),
        }
    }
}

impl std::str::FromStr for McpAuthMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_lowercase().as_str() {
            "none" => Ok(McpAuthMode::None),
            "bearer" => Ok(McpAuthMode::Bearer),
            other => Err(anyhow!(
                "invalid mcp auth mode '{}', expected 'none' or 'bearer'",
                other
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpAuthConfig {
    #[serde(default)]
    pub mode: McpAuthMode,
}

impl Default for McpAuthConfig {
    fn default() -> Self {
        Self {
            mode: McpAuthMode::None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub auth: McpAuthConfig,
    #[serde(default = "default_mcp_session_ttl_seconds")]
    pub session_ttl_seconds: u64,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            auth: McpAuthConfig::default(),
            session_ttl_seconds: default_mcp_session_ttl_seconds(),
        }
    }
}

fn default_mcp_session_ttl_seconds() -> u64 {
    3600
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum SearchBackend {
    #[serde(rename = "auto")]
    #[default]
    Auto,
    #[serde(rename = "builtin")]
    Builtin,
    #[serde(rename = "rg-fd")]
    RgFd,
}

impl Display for SearchBackend {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchBackend::Auto => write!(f, "auto"),
            SearchBackend::Builtin => write!(f, "builtin"),
            SearchBackend::RgFd => write!(f, "rg-fd"),
        }
    }
}

impl std::str::FromStr for SearchBackend {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_lowercase().as_str() {
            "auto" => Ok(SearchBackend::Auto),
            "builtin" => Ok(SearchBackend::Builtin),
            "rg-fd" => Ok(SearchBackend::RgFd),
            other => Err(anyhow!(
                "invalid search backend '{}', expected 'auto', 'builtin', or 'rg-fd'",
                other
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchConfig {
    #[serde(default)]
    pub backend: SearchBackend,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            backend: SearchBackend::Auto,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawConfig {
    #[serde(default = "default_raw_upload_max_bytes")]
    pub upload_max_bytes: u64,
    #[serde(default = "default_raw_url_timeout_seconds")]
    pub url_timeout_seconds: u64,
    #[serde(default = "default_raw_pdf_liteparse_max_pages")]
    pub pdf_liteparse_max_pages: u32,
    #[serde(default = "default_raw_pdf_liteparse_timeout_ms")]
    pub pdf_liteparse_timeout_ms: u64,
    #[serde(default = "default_raw_pdf_liteparse_mem_limit_mb")]
    pub pdf_liteparse_mem_limit_mb: u64,
}

impl Default for RawConfig {
    fn default() -> Self {
        Self {
            upload_max_bytes: default_raw_upload_max_bytes(),
            url_timeout_seconds: default_raw_url_timeout_seconds(),
            pdf_liteparse_max_pages: default_raw_pdf_liteparse_max_pages(),
            pdf_liteparse_timeout_ms: default_raw_pdf_liteparse_timeout_ms(),
            pdf_liteparse_mem_limit_mb: default_raw_pdf_liteparse_mem_limit_mb(),
        }
    }
}

fn default_raw_upload_max_bytes() -> u64 {
    50 * 1024 * 1024
}

fn default_raw_url_timeout_seconds() -> u64 {
    30
}

fn default_raw_pdf_liteparse_max_pages() -> u32 {
    30
}

fn default_raw_pdf_liteparse_timeout_ms() -> u64 {
    60_000
}

fn default_raw_pdf_liteparse_mem_limit_mb() -> u64 {
    4096
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub name: String,
    pub vault_path: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub search: SearchConfig,
    #[serde(default)]
    pub raw: RawConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        Self {
            name: "writestead".to_string(),
            vault_path: format!("{}/Documents/writestead", home),
            host: "127.0.0.1".to_string(),
            port: 8765,
            sync: SyncConfig::default(),
            mcp: McpConfig::default(),
            search: SearchConfig::default(),
            raw: RawConfig::default(),
        }
    }
}

pub fn runtime_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("WRITESTEAD_RUNTIME_DIR") {
        return PathBuf::from(custom);
    }

    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("writestead");
    }

    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".config").join("writestead")
}

pub fn config_file_path() -> PathBuf {
    if let Ok(custom) = std::env::var("WRITESTEAD_CONFIG_FILE") {
        return PathBuf::from(custom);
    }
    runtime_dir().join("config.json")
}

pub fn pid_file_path() -> PathBuf {
    if let Ok(custom) = std::env::var("WRITESTEAD_PID_FILE") {
        return PathBuf::from(custom);
    }
    runtime_dir().join("writestead.pid")
}

pub fn log_file_path() -> PathBuf {
    if let Ok(custom) = std::env::var("WRITESTEAD_LOG_FILE") {
        return PathBuf::from(custom);
    }
    runtime_dir().join("writestead.log")
}

pub fn daemon_url(cfg: &AppConfig) -> String {
    let host = if cfg.host == "0.0.0.0" {
        "127.0.0.1"
    } else {
        cfg.host.as_str()
    };
    format!("http://{}:{}", host, cfg.port)
}

pub fn ensure_runtime_dir() -> Result<()> {
    let dir = runtime_dir();
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create runtime dir {}", dir.display()))?;
    Ok(())
}

pub fn expand_tilde(path: &str) -> String {
    let input = path.trim();
    if input == "~" {
        return std::env::var("HOME").unwrap_or_else(|_| input.to_string());
    }
    if let Some(rest) = input.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "".to_string());
        if home.is_empty() {
            return input.to_string();
        }
        return format!("{}/{}", home, rest);
    }
    input.to_string()
}

pub fn load_or_default() -> Result<AppConfig> {
    let path = config_file_path();
    if !path.exists() {
        return Ok(AppConfig::default());
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let mut cfg: AppConfig = serde_json::from_str(&raw)
        .with_context(|| format!("invalid JSON in config {}", path.display()))?;

    if cfg.name.trim().is_empty() {
        cfg.name = AppConfig::default().name;
    }
    if cfg.vault_path.trim().is_empty() {
        cfg.vault_path = AppConfig::default().vault_path;
    }
    if cfg.host.trim().is_empty() {
        cfg.host = AppConfig::default().host;
    }
    if cfg.port == 0 {
        cfg.port = AppConfig::default().port;
    }
    if cfg.mcp.session_ttl_seconds == 0 {
        cfg.mcp.session_ttl_seconds = default_mcp_session_ttl_seconds();
    }
    if cfg.raw.upload_max_bytes == 0 {
        cfg.raw.upload_max_bytes = default_raw_upload_max_bytes();
    }
    if cfg.raw.url_timeout_seconds == 0 {
        cfg.raw.url_timeout_seconds = default_raw_url_timeout_seconds();
    }
    if cfg.raw.pdf_liteparse_max_pages == 0 {
        cfg.raw.pdf_liteparse_max_pages = default_raw_pdf_liteparse_max_pages();
    }
    if cfg.raw.pdf_liteparse_timeout_ms == 0 {
        cfg.raw.pdf_liteparse_timeout_ms = default_raw_pdf_liteparse_timeout_ms();
    }
    if cfg.raw.pdf_liteparse_mem_limit_mb == 0 {
        cfg.raw.pdf_liteparse_mem_limit_mb = default_raw_pdf_liteparse_mem_limit_mb();
    }

    Ok(cfg)
}

pub fn save(cfg: &AppConfig) -> Result<()> {
    let path = config_file_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    }

    let body = serde_json::to_string_pretty(cfg)?;
    fs::write(&path, format!("{}\n", body))
        .with_context(|| format!("failed to write config {}", path.display()))?;
    Ok(())
}

pub fn effective_mcp_auth_mode(cfg: &AppConfig) -> McpAuthMode {
    if let Ok(raw) = std::env::var("WRITESTEAD_MCP_AUTH_MODE") {
        if let Ok(mode) = raw.parse::<McpAuthMode>() {
            return mode;
        }
    }
    cfg.mcp.auth.mode.clone()
}

pub fn effective_mcp_bearer_token(_cfg: &AppConfig) -> Option<String> {
    if let Ok(raw) = std::env::var("WRITESTEAD_BEARER_TOKEN") {
        let value = raw.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

pub fn get_value(cfg: &AppConfig, key: &str) -> Result<serde_json::Value> {
    match key {
        "name" => Ok(json!(cfg.name)),
        "vault_path" => Ok(json!(cfg.vault_path)),
        "host" => Ok(json!(cfg.host)),
        "port" => Ok(json!(cfg.port)),
        "sync.backend" => Ok(json!(cfg.sync.backend.to_string())),
        "mcp.auth.mode" => Ok(json!(cfg.mcp.auth.mode.to_string())),
        "mcp.session_ttl_seconds" => Ok(json!(cfg.mcp.session_ttl_seconds)),
        "search.backend" => Ok(json!(cfg.search.backend.to_string())),
        "raw.upload_max_bytes" => Ok(json!(cfg.raw.upload_max_bytes)),
        "raw.url_timeout_seconds" => Ok(json!(cfg.raw.url_timeout_seconds)),
        "raw.pdf_liteparse_max_pages" => Ok(json!(cfg.raw.pdf_liteparse_max_pages)),
        "raw.pdf_liteparse_timeout_ms" => Ok(json!(cfg.raw.pdf_liteparse_timeout_ms)),
        "raw.pdf_liteparse_mem_limit_mb" => Ok(json!(cfg.raw.pdf_liteparse_mem_limit_mb)),
        _ => Err(anyhow!(
            "unknown config key '{}': use name|vault_path|host|port|sync.backend|mcp.auth.mode|mcp.session_ttl_seconds|search.backend|raw.upload_max_bytes|raw.url_timeout_seconds|raw.pdf_liteparse_max_pages|raw.pdf_liteparse_timeout_ms|raw.pdf_liteparse_mem_limit_mb",
            key
        )),
    }
}

pub fn set_value(cfg: &mut AppConfig, key: &str, value: &str) -> Result<()> {
    match key {
        "name" => cfg.name = value.trim().to_string(),
        "vault_path" => cfg.vault_path = expand_tilde(value),
        "host" => cfg.host = value.trim().to_string(),
        "port" => {
            cfg.port = value
                .trim()
                .parse::<u16>()
                .with_context(|| format!("invalid port '{}'", value))?
        }
        "sync.backend" => cfg.sync.backend = value.parse::<SyncBackend>()?,
        "mcp.auth.mode" => cfg.mcp.auth.mode = value.parse::<McpAuthMode>()?,
        "mcp.session_ttl_seconds" => {
            cfg.mcp.session_ttl_seconds = value
                .trim()
                .parse::<u64>()
                .with_context(|| format!("invalid mcp.session_ttl_seconds '{}'", value))?;
            if cfg.mcp.session_ttl_seconds == 0 {
                cfg.mcp.session_ttl_seconds = default_mcp_session_ttl_seconds();
            }
        }
        "search.backend" => cfg.search.backend = value.parse::<SearchBackend>()?,
        "raw.upload_max_bytes" => {
            cfg.raw.upload_max_bytes = value
                .trim()
                .parse::<u64>()
                .with_context(|| format!("invalid raw.upload_max_bytes '{}'", value))?;
            if cfg.raw.upload_max_bytes == 0 {
                cfg.raw.upload_max_bytes = default_raw_upload_max_bytes();
            }
        }
        "raw.url_timeout_seconds" => {
            cfg.raw.url_timeout_seconds = value
                .trim()
                .parse::<u64>()
                .with_context(|| format!("invalid raw.url_timeout_seconds '{}'", value))?;
            if cfg.raw.url_timeout_seconds == 0 {
                cfg.raw.url_timeout_seconds = default_raw_url_timeout_seconds();
            }
        }
        "raw.pdf_liteparse_max_pages" => {
            cfg.raw.pdf_liteparse_max_pages = value
                .trim()
                .parse::<u32>()
                .with_context(|| format!("invalid raw.pdf_liteparse_max_pages '{}'", value))?;
            if cfg.raw.pdf_liteparse_max_pages == 0 {
                cfg.raw.pdf_liteparse_max_pages = default_raw_pdf_liteparse_max_pages();
            }
        }
        "raw.pdf_liteparse_timeout_ms" => {
            cfg.raw.pdf_liteparse_timeout_ms = value
                .trim()
                .parse::<u64>()
                .with_context(|| format!("invalid raw.pdf_liteparse_timeout_ms '{}'", value))?;
            if cfg.raw.pdf_liteparse_timeout_ms == 0 {
                cfg.raw.pdf_liteparse_timeout_ms = default_raw_pdf_liteparse_timeout_ms();
            }
        }
        "raw.pdf_liteparse_mem_limit_mb" => {
            cfg.raw.pdf_liteparse_mem_limit_mb = value
                .trim()
                .parse::<u64>()
                .with_context(|| format!("invalid raw.pdf_liteparse_mem_limit_mb '{}'", value))?;
            if cfg.raw.pdf_liteparse_mem_limit_mb == 0 {
                cfg.raw.pdf_liteparse_mem_limit_mb = default_raw_pdf_liteparse_mem_limit_mb();
            }
        }
        "mcp.auth.bearer_token" => {
            return Err(anyhow!(
                "mcp.auth.bearer_token is disabled; use WRITESTEAD_BEARER_TOKEN env var"
            ))
        }
        _ => {
            return Err(anyhow!(
                "unknown config key '{}': use name|vault_path|host|port|sync.backend|mcp.auth.mode|mcp.session_ttl_seconds|search.backend|raw.upload_max_bytes|raw.url_timeout_seconds|raw.pdf_liteparse_max_pages|raw.pdf_liteparse_timeout_ms|raw.pdf_liteparse_mem_limit_mb",
                key
            ))
        }
    }

    Ok(())
}

pub fn unset_value(cfg: &mut AppConfig, key: &str) -> Result<()> {
    let defaults = AppConfig::default();

    match key {
        "name" => cfg.name = defaults.name,
        "vault_path" => cfg.vault_path = defaults.vault_path,
        "host" => cfg.host = defaults.host,
        "port" => cfg.port = defaults.port,
        "sync.backend" => cfg.sync.backend = defaults.sync.backend,
        "mcp.auth.mode" => cfg.mcp.auth.mode = defaults.mcp.auth.mode,
        "mcp.session_ttl_seconds" => cfg.mcp.session_ttl_seconds = defaults.mcp.session_ttl_seconds,
        "search.backend" => cfg.search.backend = defaults.search.backend,
        "raw.upload_max_bytes" => cfg.raw.upload_max_bytes = defaults.raw.upload_max_bytes,
        "raw.url_timeout_seconds" => cfg.raw.url_timeout_seconds = defaults.raw.url_timeout_seconds,
        "raw.pdf_liteparse_max_pages" => {
            cfg.raw.pdf_liteparse_max_pages = defaults.raw.pdf_liteparse_max_pages
        }
        "raw.pdf_liteparse_timeout_ms" => {
            cfg.raw.pdf_liteparse_timeout_ms = defaults.raw.pdf_liteparse_timeout_ms
        }
        "raw.pdf_liteparse_mem_limit_mb" => {
            cfg.raw.pdf_liteparse_mem_limit_mb = defaults.raw.pdf_liteparse_mem_limit_mb
        }
        "mcp.auth.bearer_token" => {
            return Err(anyhow!(
                "mcp.auth.bearer_token is disabled; use WRITESTEAD_BEARER_TOKEN env var"
            ))
        }
        _ => {
            return Err(anyhow!(
                "unknown config key '{}': use name|vault_path|host|port|sync.backend|mcp.auth.mode|mcp.session_ttl_seconds|search.backend|raw.upload_max_bytes|raw.url_timeout_seconds|raw.pdf_liteparse_max_pages|raw.pdf_liteparse_timeout_ms|raw.pdf_liteparse_mem_limit_mb",
                key
            ))
        }
    }

    Ok(())
}
