//! Configuration module for BlipMQ.
//!
//! Loads a structured TOML file into strongly‐typed structs (`Config`, `ServerConfig`, etc.)
//! using `serde` + `toml`.
//!
//! # Example `blipmq.toml`
//! ```toml
//! [server]
//! bind_addr        = "0.0.0.0:6379"
//! max_connections  = 10_000
//!
//! [auth]
//! api_keys = ["secret-key-1", "secret-key-2"]
//!
//! [queues]
//! default_ttl_ms    = 86_400_000  # 24h
//! max_queue_depth   = 1_000_000
//! overflow_policy   = "drop_oldest"
//!
//! [wal]
//! directory          = "./data/wal"
//! segment_size_bytes = 134_217_728   # 128 MiB
//! flush_interval_ms  = 50            # ms
//!
//! [metrics]
//! bind_addr = "127.0.0.1:9100"
//!
//! [delivery]
//! max_batch = 32             # maximum messages per flush
//! ```
//! # Usage
//! ```rust
//! let cfg = blipmq::config::load_config("./blipmq.toml")?;
//! println!("Broker listening on {}", cfg.server.bind_addr);
//! println!("Delivery max_batch = {}", cfg.delivery.max_batch);
//! ```

use serde::Deserialize;
use std::{fs, path::Path};

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub bind_addr: String,
    pub max_connections: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AuthConfig {
    /// API keys allowed to publish/subscribe. Empty = no auth.
    #[serde(default)]
    pub api_keys: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct QueueConfig {
    pub default_ttl_ms: u64,
    pub max_queue_depth: usize,
    /// Overflow policy when queue is full: "drop_oldest" | "drop_new" | "block"
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

/// Controls batching and flush behavior in subscriber delivery.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct DeliveryConfig {
    /// Maximum number of messages to coalesce per flush.
    pub max_batch: usize,
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        DeliveryConfig { max_batch: 64 }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub queues: QueueConfig,
    pub wal: WalConfig,
    pub metrics: MetricsConfig,

    /// Subscriber delivery tuning parameters.
    /// If omitted in TOML, defaults will be applied.
    #[serde(default)]
    pub delivery: DeliveryConfig,
}

/// Load configuration from a TOML file into `Config`.
pub fn load_config<P: AsRef<Path>>(path: P) -> Result<Config, anyhow::Error> {
    let raw: String = fs::read_to_string(&path)?;
    let cfg: Config = toml::from_str(&raw)?;
    Ok(cfg)
}
