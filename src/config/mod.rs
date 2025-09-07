//! Configuration module for BlipMQ.
//!
//! Loads a structured TOML file into strongly‐typed structs (`Config`, `ServerConfig`, etc.)
//! using `serde` + `toml`.
//! Default values are provided via `#[serde(default)]` and `impl Default` where appropriate.

use once_cell::sync::Lazy;
use serde::Deserialize;
use std::{fs, path::Path};

use crate::core::queue::qos0::OverflowPolicy;

/// Server listen address & connection limits
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ServerConfig {
    pub bind_addr: String,
    pub max_connections: usize,
    pub max_message_size_bytes: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            bind_addr: "127.0.0.1:7878".into(),
            max_connections: 256,
            max_message_size_bytes: 1024 * 1024, // 1MB default
        }
    }
}

/// API‐key based authentication
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
pub struct AuthConfig {
    /// API keys allowed to publish/subscribe. Empty = no auth.
    pub api_keys: Vec<String>,
}

/// Topic‐level & subscriber‐level queue settings
#[derive(Debug, Deserialize, Clone)]
pub struct QueueConfig {
    /// Flume capacity for each topic input queue
    #[serde(alias = "max_queue_depth")]
    pub topic_capacity: usize,

    /// ArrayQueue capacity for each subscriber
    pub subscriber_capacity: usize,

    /// Overflow policy: "drop_oldest" | "drop_new" | "block"
    pub overflow_policy: OverflowPolicy,
}

/// Write‐Ahead‐Log settings
#[derive(Debug, Deserialize, Clone)]
pub struct WalConfig {
    pub directory: String,
    pub segment_size_bytes: usize,
    pub flush_interval_ms: u64,
}

/// Metrics endpoint
#[derive(Debug, Deserialize, Clone)]
pub struct MetricsConfig {
    pub bind_addr: String,
}

/// Subscriber flush parameters & per‐message TTL
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct DeliveryConfig {
    /// Max messages per flush batch
    pub max_batch: usize,

    /// Byte budget for a single network flush from a writer task
    pub max_batch_bytes: usize,

    /// Time-based flush for writer tasks (milliseconds)
    pub flush_interval_ms: u64,

    /// Optional number of fanout shards per topic (0 = auto)
    pub fanout_shards: usize,

    /// Default TTL for each new message (ms)
    pub default_ttl_ms: u64,
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        DeliveryConfig {
            max_batch: 64,
            max_batch_bytes: 256 * 1024, // 256 KiB
            flush_interval_ms: 1,        // ~1ms latency target
            fanout_shards: 0,            // auto
            default_ttl_ms: 0,
        }
    }
}

/// Top‐level BlipMQ configuration
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    pub queues: QueueConfig,
    pub wal: WalConfig,
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub delivery: DeliveryConfig,
}

/// Global, lazily‐loaded config instance from `blipmq.toml`
pub static CONFIG: Lazy<Config> = Lazy::new(|| {
    let toml_str =
        fs::read_to_string("blipmq.toml").expect("Could not find blipmq.toml in working directory");
    toml::from_str(&toml_str).expect("Invalid blipmq.toml format")
});

/// Convenience loader if you need a custom path
pub fn load_config<P: AsRef<Path>>(path: P) -> Result<Config, anyhow::Error> {
    let raw = fs::read_to_string(path)?;
    Ok(toml::from_str(&raw)?)
}
