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
#[serde(default)]
pub struct QueueConfig {
    /// Flume capacity for each topic input queue
    #[serde(alias = "max_queue_depth")]
    pub topic_capacity: usize,

    /// ArrayQueue capacity for each subscriber
    pub subscriber_capacity: usize,

    /// Overflow policy: "drop_oldest" | "drop_new" | "block"
    pub overflow_policy: OverflowPolicy,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            topic_capacity: 1000,
            subscriber_capacity: 512,
            overflow_policy: OverflowPolicy::default(),
        }
    }
}

/// Write‐Ahead‐Log settings
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct WalConfig {
    pub directory: String,
    pub segment_size_bytes: usize,
    pub flush_interval_ms: u64,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            directory: "./wal".to_string(),
            segment_size_bytes: 1024 * 1024,
            flush_interval_ms: 5000,
        }
    }
}

/// Metrics endpoint
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct MetricsConfig {
    pub bind_addr: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:9090".to_string(),
        }
    }
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

/// High-performance settings for production deployment
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct PerformanceConfig {
    /// Enable timer wheel for TTL management
    pub enable_timer_wheel: bool,
    
    /// Timer wheel resolution in milliseconds
    pub timer_wheel_resolution_ms: u64,
    
    /// Enable memory pooling for frequently allocated objects
    pub enable_memory_pools: bool,
    
    /// Memory pool warming on startup
    pub warm_up_pools: bool,
    
    /// Enable batch processing for various operations
    pub enable_batch_processing: bool,
    
    /// Batch processing configuration
    pub batch_config: BatchProcessingConfig,
    
    /// CPU affinity settings (platform-specific)
    pub cpu_affinity: CpuAffinityConfig,
    
    /// Advanced network optimizations
    pub network_optimizations: NetworkOptimizationConfig,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            enable_timer_wheel: true,
            timer_wheel_resolution_ms: 10,
            enable_memory_pools: true,
            warm_up_pools: true,
            enable_batch_processing: true,
            batch_config: BatchProcessingConfig::default(),
            cpu_affinity: CpuAffinityConfig::default(),
            network_optimizations: NetworkOptimizationConfig::default(),
        }
    }
}

/// Batch processing configuration
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct BatchProcessingConfig {
    /// Maximum batch size for TTL processing
    pub ttl_max_batch_size: usize,
    
    /// Maximum delay before processing partial TTL batch (ms)
    pub ttl_max_batch_delay_ms: u64,
    
    /// Maximum batch size for fanout processing
    pub fanout_max_batch_size: usize,
    
    /// Maximum delay before processing partial fanout batch (ms)
    pub fanout_max_batch_delay_ms: u64,
    
    /// Channel capacity for batch processing queues
    pub channel_capacity: usize,
}

impl Default for BatchProcessingConfig {
    fn default() -> Self {
        Self {
            ttl_max_batch_size: 1000,
            ttl_max_batch_delay_ms: 10,
            fanout_max_batch_size: 2000,
            fanout_max_batch_delay_ms: 5,
            channel_capacity: 16384,
        }
    }
}

/// CPU affinity configuration for thread pinning
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct CpuAffinityConfig {
    /// Enable CPU affinity for worker threads
    pub enable_affinity: bool,
    
    /// Pin fanout workers to specific cores
    pub pin_fanout_workers: bool,
    
    /// Pin timer wheel tasks to specific cores
    pub pin_timer_tasks: bool,
    
    /// Pin network I/O tasks to specific cores
    pub pin_network_tasks: bool,
    
    /// Core offset for thread pinning (0-based)
    pub core_offset: usize,
}

impl Default for CpuAffinityConfig {
    fn default() -> Self {
        Self {
            enable_affinity: cfg!(feature = "affinity"),
            pin_fanout_workers: true,
            pin_timer_tasks: true,
            pin_network_tasks: true,
            core_offset: 0,
        }
    }
}

/// Network optimization settings
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct NetworkOptimizationConfig {
    /// Enable TCP_NODELAY for low latency
    pub tcp_nodelay: bool,
    
    /// Socket send buffer size (bytes)
    pub send_buffer_size: usize,
    
    /// Socket receive buffer size (bytes)
    pub recv_buffer_size: usize,
    
    /// Enable SO_REUSEADDR
    pub reuse_addr: bool,
    
    /// Enable SO_REUSEPORT (Linux only)
    pub reuse_port: bool,
    
    /// Connection keep-alive timeout (seconds)
    pub keepalive_timeout_secs: u64,
}

impl Default for NetworkOptimizationConfig {
    fn default() -> Self {
        Self {
            tcp_nodelay: true,
            send_buffer_size: 64 * 1024,     // 64KB
            recv_buffer_size: 64 * 1024,     // 64KB
            reuse_addr: true,
            reuse_port: false,               // Platform specific
            keepalive_timeout_secs: 300,     // 5 minutes
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
    #[serde(default)]
    pub queues: QueueConfig,
    #[serde(default)]
    pub wal: WalConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub delivery: DeliveryConfig,
    #[serde(default)]
    pub performance: PerformanceConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            auth: AuthConfig::default(),
            queues: QueueConfig::default(),
            wal: WalConfig::default(),
            metrics: MetricsConfig::default(),
            delivery: DeliveryConfig::default(),
            performance: PerformanceConfig::default(),
        }
    }
}

/// Global, lazily‐loaded config instance from `blipmq.toml`
pub static CONFIG: Lazy<Config> = Lazy::new(|| {
    let config_path = std::env::var("BLIPMQ_CONFIG").unwrap_or_else(|_| "blipmq.toml".to_string());
    match load_config(&config_path) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Failed to load config from {}: {}", config_path, e);
            eprintln!("Using default configuration");
            Config::default()
        }
    }
});

/// Convenience loader if you need a custom path
pub fn load_config<P: AsRef<Path>>(path: P) -> Result<Config, anyhow::Error> {
    let raw = fs::read_to_string(path)?;
    Ok(toml::from_str(&raw)?)
}
