use std::env;
use std::fs;
use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
    pub port: u16,
    pub metrics_addr: String,
    pub metrics_port: u16,
    pub wal_path: String,
    pub fsync_policy: Option<String>,
    pub max_retries: u32,
    pub retry_backoff_ms: u64,
    pub allowed_api_keys: Vec<String>,
    pub enable_tokio_console: bool,
}

#[derive(Debug, Deserialize, Clone, Default)]
struct FileConfig {
    pub bind_addr: Option<String>,
    pub port: Option<u16>,
    pub metrics_addr: Option<String>,
    pub metrics_port: Option<u16>,
    pub wal_path: Option<String>,
    pub fsync_policy: Option<String>,
    pub max_retries: Option<u32>,
    pub retry_backoff_ms: Option<u64>,
    pub allowed_api_keys: Option<Vec<String>>,
    pub enable_tokio_console: Option<bool>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("YAML parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("config parse error: {0}")]
    Parse(String),
}

impl Config {
    fn load_file<P: AsRef<Path>>(path: P) -> Result<FileConfig, ConfigError> {
        let path_ref = path.as_ref();
        let raw = fs::read_to_string(path_ref)?;
        let ext = path_ref
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("toml")
            .to_ascii_lowercase();

        if ext == "yaml" || ext == "yml" {
            let cfg: FileConfig = serde_yaml::from_str(&raw)?;
            Ok(cfg)
        } else {
            let cfg: FileConfig = toml::from_str(&raw)?;
            Ok(cfg)
        }
    }

    /// Load configuration from an optional file path and environment variables.
    ///
    /// Precedence: file values provide defaults, environment variables override.
    pub fn load(path: Option<&str>) -> Result<Self, ConfigError> {
        let env_path = env::var("BLIPMQ_CONFIG").ok();
        let effective_path = path.map(|s| s.to_string()).or(env_path);

        let file_cfg = if let Some(p) = effective_path {
            Self::load_file(p)?
        } else {
            FileConfig::default()
        };

        // File defaults.
        let mut bind_addr = file_cfg
            .bind_addr
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let mut port = file_cfg.port.unwrap_or(5555);
        let mut metrics_addr = file_cfg
            .metrics_addr
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let mut metrics_port = file_cfg.metrics_port.unwrap_or(9100);
        let mut wal_path = file_cfg
            .wal_path
            .unwrap_or_else(|| "blipmq.wal".to_string());
        let mut fsync_policy = file_cfg.fsync_policy;
        let mut max_retries = file_cfg.max_retries.unwrap_or(3);
        let mut retry_backoff_ms = file_cfg.retry_backoff_ms.unwrap_or(100);
        let mut allowed_api_keys = file_cfg.allowed_api_keys.unwrap_or_else(Vec::new);
        let mut enable_tokio_console = file_cfg.enable_tokio_console.unwrap_or(false);

        // Env overrides.
        if let Ok(v) = env::var("BLIPMQ_BIND_ADDR") {
            bind_addr = v;
        }

        if let Ok(v) = env::var("BLIPMQ_PORT") {
            port = v
                .parse()
                .map_err(|e| ConfigError::Parse(format!("BLIPMQ_PORT: {e}")))?;
        }

        if let Ok(v) = env::var("BLIPMQ_METRICS_ADDR") {
            metrics_addr = v;
        }

        if let Ok(v) = env::var("BLIPMQ_METRICS_PORT") {
            metrics_port = v
                .parse()
                .map_err(|e| ConfigError::Parse(format!("BLIPMQ_METRICS_PORT: {e}")))?;
        }

        if let Ok(v) = env::var("BLIPMQ_WAL_PATH") {
            wal_path = v;
        }

        if let Ok(v) = env::var("BLIPMQ_FSYNC_POLICY") {
            fsync_policy = Some(v);
        }

        if let Ok(v) = env::var("BLIPMQ_MAX_RETRIES") {
            max_retries = v
                .parse()
                .map_err(|e| ConfigError::Parse(format!("BLIPMQ_MAX_RETRIES: {e}")))?;
        }

        if let Ok(v) = env::var("BLIPMQ_RETRY_BACKOFF_MS") {
            retry_backoff_ms = v
                .parse()
                .map_err(|e| ConfigError::Parse(format!("BLIPMQ_RETRY_BACKOFF_MS: {e}")))?;
        }

        if let Ok(v) = env::var("BLIPMQ_ALLOWED_API_KEYS") {
            let keys: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            allowed_api_keys = keys;
        }

        if let Ok(v) = env::var("BLIPMQ_ENABLE_TOKIO_CONSOLE") {
            enable_tokio_console =
                matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
        }

        Ok(Config {
            bind_addr,
            port,
            metrics_addr,
            metrics_port,
            wal_path,
            fsync_policy,
            max_retries,
            retry_backoff_ms,
            allowed_api_keys,
            enable_tokio_console,
        })
    }
}
