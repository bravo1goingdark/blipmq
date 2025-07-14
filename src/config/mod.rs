use serde::Deserialize;
use std::{fs, path::Path};

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub bind_addr: String,
    pub max_connections: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AuthConfig {
    pub api_keys: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct QueueConfig {
    pub default_ttl_ms: u64,
    pub max_queue_depth: usize,
    pub overflow_policy: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WalConfig {
    pub directory: String,
    pub segment_size_bytes: usize,
    pub flush_interval_ms: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MetricsConfig {
    pub bind_addr: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub queues: QueueConfig,
    pub wal: WalConfig,
    pub metrics: MetricsConfig,
}

pub fn load_config<P: AsRef<Path>>(path: P) -> Result<Config, anyhow::Error> {
    let raw : String = fs::read_to_string(path)?;
    let config: Config = toml::from_str(&raw)?;
    Ok(config)
}
