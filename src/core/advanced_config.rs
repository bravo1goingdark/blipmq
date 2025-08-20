//! Advanced configuration with production-grade defaults and auto-tuning.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Production-optimized configuration profiles
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductionConfig {
    pub profile: PerformanceProfile,
    pub server: AdvancedServerConfig,
    pub memory: MemoryConfig,
    pub networking: NetworkConfig,
    pub performance: PerformanceConfig,
    pub monitoring: MonitoringConfig,
    pub security: SecurityConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PerformanceProfile {
    /// Low-latency optimizations, higher resource usage
    LowLatency,
    /// High-throughput optimizations, batching enabled
    HighThroughput,
    /// Balanced performance for general use
    Balanced,
    /// Resource-efficient for constrained environments
    ResourceEfficient,
    /// Custom profile with manual tuning
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvancedServerConfig {
    pub bind_addr: String,
    pub max_connections: usize,
    pub max_connections_per_ip: usize,
    pub connection_timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub tcp_nodelay: bool,
    pub tcp_keepalive: bool,
    pub tcp_keepalive_interval_ms: u64,
    pub socket_recv_buffer_size: Option<usize>,
    pub socket_send_buffer_size: Option<usize>,
    pub backlog: u32,
    pub reuse_port: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    pub enable_memory_pooling: bool,
    pub small_buffer_pool_size: usize,
    pub medium_buffer_pool_size: usize,
    pub large_buffer_pool_size: usize,
    pub small_buffer_capacity: usize,
    pub medium_buffer_capacity: usize,
    pub large_buffer_capacity: usize,
    pub message_pool_size: usize,
    pub enable_zero_copy: bool,
    pub max_message_size: usize,
    pub memory_limit_mb: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub enable_vectored_io: bool,
    pub max_batch_size: usize,
    pub max_batch_count: usize,
    pub write_buffer_size: usize,
    pub read_buffer_size: usize,
    pub enable_compression: bool,
    pub compression_threshold: usize,
    pub enable_tls: bool,
    pub tls_cert_path: Option<String>,
    pub tls_key_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceConfig {
    pub worker_threads: usize,
    pub enable_thread_affinity: bool,
    pub thread_stack_size_kb: usize,
    pub enable_numa_awareness: bool,
    pub gc_threshold_mb: usize,
    pub enable_jemalloc: bool,
    pub prefetch_distance: usize,
    pub enable_branch_prediction: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitoringConfig {
    pub enable_metrics: bool,
    pub metrics_bind_addr: String,
    pub enable_tracing: bool,
    pub tracing_level: String,
    pub enable_profiling: bool,
    pub profiling_sample_rate: f64,
    pub metrics_collection_interval_ms: u64,
    pub enable_health_checks: bool,
    pub health_check_interval_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    pub enable_authentication: bool,
    pub api_keys: Vec<String>,
    pub enable_rate_limiting: bool,
    pub rate_limit_per_connection: u32,
    pub rate_limit_per_ip: u32,
    pub rate_limit_window_ms: u64,
    pub enable_ddos_protection: bool,
    pub max_payload_size: usize,
    pub enable_audit_logging: bool,
}

impl Default for ProductionConfig {
    fn default() -> Self {
        Self::balanced_profile()
    }
}

impl ProductionConfig {
    /// Low-latency profile for financial trading, gaming, etc.
    pub fn low_latency_profile() -> Self {
        Self {
            profile: PerformanceProfile::LowLatency,
            server: AdvancedServerConfig {
                bind_addr: "0.0.0.0:8080".to_string(),
                max_connections: 5000,
                max_connections_per_ip: 50,
                connection_timeout_ms: 5000,
                idle_timeout_ms: 30000,
                tcp_nodelay: true,
                tcp_keepalive: true,
                tcp_keepalive_interval_ms: 1000,
                socket_recv_buffer_size: Some(256 * 1024),
                socket_send_buffer_size: Some(256 * 1024),
                backlog: 1024,
                reuse_port: true,
            },
            memory: MemoryConfig {
                enable_memory_pooling: true,
                small_buffer_pool_size: 200,
                medium_buffer_pool_size: 100,
                large_buffer_pool_size: 50,
                small_buffer_capacity: 2048,
                medium_buffer_capacity: 8192,
                large_buffer_capacity: 32768,
                message_pool_size: 2000,
                enable_zero_copy: true,
                max_message_size: 64 * 1024,
                memory_limit_mb: Some(2048),
            },
            networking: NetworkConfig {
                enable_vectored_io: true,
                max_batch_size: 8192,
                max_batch_count: 4, // Small batches for low latency
                write_buffer_size: 16384,
                read_buffer_size: 16384,
                enable_compression: false, // Disabled for latency
                compression_threshold: 0,
                enable_tls: false,
                tls_cert_path: None,
                tls_key_path: None,
            },
            performance: PerformanceConfig {
                worker_threads: num_cpus::get(),
                enable_thread_affinity: true,
                thread_stack_size_kb: 512,
                enable_numa_awareness: true,
                gc_threshold_mb: 512,
                enable_jemalloc: true,
                prefetch_distance: 64,
                enable_branch_prediction: true,
            },
            monitoring: MonitoringConfig {
                enable_metrics: true,
                metrics_bind_addr: "127.0.0.1:9090".to_string(),
                enable_tracing: false, // Disabled for performance
                tracing_level: "error".to_string(),
                enable_profiling: false,
                profiling_sample_rate: 0.01,
                metrics_collection_interval_ms: 1000,
                enable_health_checks: true,
                health_check_interval_ms: 5000,
            },
            security: SecurityConfig {
                enable_authentication: false, // Often disabled in low-latency scenarios
                api_keys: vec![],
                enable_rate_limiting: true,
                rate_limit_per_connection: 10000,
                rate_limit_per_ip: 50000,
                rate_limit_window_ms: 1000,
                enable_ddos_protection: true,
                max_payload_size: 64 * 1024,
                enable_audit_logging: false,
            },
        }
    }
    
    /// High-throughput profile for data processing, analytics
    pub fn high_throughput_profile() -> Self {
        Self {
            profile: PerformanceProfile::HighThroughput,
            server: AdvancedServerConfig {
                bind_addr: "0.0.0.0:8080".to_string(),
                max_connections: 20000,
                max_connections_per_ip: 200,
                connection_timeout_ms: 30000,
                idle_timeout_ms: 300000,
                tcp_nodelay: false, // Enable batching
                tcp_keepalive: true,
                tcp_keepalive_interval_ms: 10000,
                socket_recv_buffer_size: Some(1024 * 1024),
                socket_send_buffer_size: Some(1024 * 1024),
                backlog: 4096,
                reuse_port: true,
            },
            memory: MemoryConfig {
                enable_memory_pooling: true,
                small_buffer_pool_size: 500,
                medium_buffer_pool_size: 200,
                large_buffer_pool_size: 100,
                small_buffer_capacity: 4096,
                medium_buffer_capacity: 16384,
                large_buffer_capacity: 65536,
                message_pool_size: 5000,
                enable_zero_copy: true,
                max_message_size: 1024 * 1024,
                memory_limit_mb: Some(8192),
            },
            networking: NetworkConfig {
                enable_vectored_io: true,
                max_batch_size: 256 * 1024,
                max_batch_count: 64, // Large batches for throughput
                write_buffer_size: 128 * 1024,
                read_buffer_size: 128 * 1024,
                enable_compression: true,
                compression_threshold: 1024,
                enable_tls: false,
                tls_cert_path: None,
                tls_key_path: None,
            },
            performance: PerformanceConfig {
                worker_threads: num_cpus::get() * 2,
                enable_thread_affinity: true,
                thread_stack_size_kb: 1024,
                enable_numa_awareness: true,
                gc_threshold_mb: 2048,
                enable_jemalloc: true,
                prefetch_distance: 256,
                enable_branch_prediction: true,
            },
            monitoring: MonitoringConfig {
                enable_metrics: true,
                metrics_bind_addr: "127.0.0.1:9090".to_string(),
                enable_tracing: true,
                tracing_level: "info".to_string(),
                enable_profiling: true,
                profiling_sample_rate: 0.1,
                metrics_collection_interval_ms: 5000,
                enable_health_checks: true,
                health_check_interval_ms: 30000,
            },
            security: SecurityConfig {
                enable_authentication: true,
                api_keys: vec!["default-key".to_string()],
                enable_rate_limiting: true,
                rate_limit_per_connection: 5000,
                rate_limit_per_ip: 25000,
                rate_limit_window_ms: 1000,
                enable_ddos_protection: true,
                max_payload_size: 1024 * 1024,
                enable_audit_logging: true,
            },
        }
    }
    
    /// Balanced profile for general production use
    pub fn balanced_profile() -> Self {
        Self {
            profile: PerformanceProfile::Balanced,
            server: AdvancedServerConfig {
                bind_addr: "0.0.0.0:8080".to_string(),
                max_connections: 10000,
                max_connections_per_ip: 100,
                connection_timeout_ms: 15000,
                idle_timeout_ms: 180000,
                tcp_nodelay: true,
                tcp_keepalive: true,
                tcp_keepalive_interval_ms: 5000,
                socket_recv_buffer_size: Some(512 * 1024),
                socket_send_buffer_size: Some(512 * 1024),
                backlog: 2048,
                reuse_port: true,
            },
            memory: MemoryConfig {
                enable_memory_pooling: true,
                small_buffer_pool_size: 150,
                medium_buffer_pool_size: 75,
                large_buffer_pool_size: 25,
                small_buffer_capacity: 2048,
                medium_buffer_capacity: 8192,
                large_buffer_capacity: 32768,
                message_pool_size: 1500,
                enable_zero_copy: true,
                max_message_size: 256 * 1024,
                memory_limit_mb: Some(4096),
            },
            networking: NetworkConfig {
                enable_vectored_io: true,
                max_batch_size: 64 * 1024,
                max_batch_count: 16,
                write_buffer_size: 64 * 1024,
                read_buffer_size: 64 * 1024,
                enable_compression: true,
                compression_threshold: 4096,
                enable_tls: false,
                tls_cert_path: None,
                tls_key_path: None,
            },
            performance: PerformanceConfig {
                worker_threads: num_cpus::get(),
                enable_thread_affinity: false,
                thread_stack_size_kb: 512,
                enable_numa_awareness: false,
                gc_threshold_mb: 1024,
                enable_jemalloc: false,
                prefetch_distance: 64,
                enable_branch_prediction: false,
            },
            monitoring: MonitoringConfig {
                enable_metrics: true,
                metrics_bind_addr: "127.0.0.1:9090".to_string(),
                enable_tracing: true,
                tracing_level: "info".to_string(),
                enable_profiling: false,
                profiling_sample_rate: 0.01,
                metrics_collection_interval_ms: 10000,
                enable_health_checks: true,
                health_check_interval_ms: 30000,
            },
            security: SecurityConfig {
                enable_authentication: true,
                api_keys: vec!["supersecretkey".to_string()],
                enable_rate_limiting: true,
                rate_limit_per_connection: 1000,
                rate_limit_per_ip: 5000,
                rate_limit_window_ms: 1000,
                enable_ddos_protection: true,
                max_payload_size: 256 * 1024,
                enable_audit_logging: false,
            },
        }
    }
    
    /// Resource-efficient profile for cloud/container deployments
    pub fn resource_efficient_profile() -> Self {
        Self {
            profile: PerformanceProfile::ResourceEfficient,
            server: AdvancedServerConfig {
                bind_addr: "0.0.0.0:8080".to_string(),
                max_connections: 2000,
                max_connections_per_ip: 20,
                connection_timeout_ms: 10000,
                idle_timeout_ms: 120000,
                tcp_nodelay: true,
                tcp_keepalive: true,
                tcp_keepalive_interval_ms: 30000,
                socket_recv_buffer_size: Some(64 * 1024),
                socket_send_buffer_size: Some(64 * 1024),
                backlog: 512,
                reuse_port: false,
            },
            memory: MemoryConfig {
                enable_memory_pooling: true,
                small_buffer_pool_size: 50,
                medium_buffer_pool_size: 25,
                large_buffer_pool_size: 10,
                small_buffer_capacity: 1024,
                medium_buffer_capacity: 4096,
                large_buffer_capacity: 16384,
                message_pool_size: 500,
                enable_zero_copy: false,
                max_message_size: 64 * 1024,
                memory_limit_mb: Some(512),
            },
            networking: NetworkConfig {
                enable_vectored_io: false,
                max_batch_size: 8192,
                max_batch_count: 4,
                write_buffer_size: 8192,
                read_buffer_size: 8192,
                enable_compression: true,
                compression_threshold: 512,
                enable_tls: false,
                tls_cert_path: None,
                tls_key_path: None,
            },
            performance: PerformanceConfig {
                worker_threads: 2,
                enable_thread_affinity: false,
                thread_stack_size_kb: 256,
                enable_numa_awareness: false,
                gc_threshold_mb: 128,
                enable_jemalloc: false,
                prefetch_distance: 32,
                enable_branch_prediction: false,
            },
            monitoring: MonitoringConfig {
                enable_metrics: true,
                metrics_bind_addr: "127.0.0.1:9090".to_string(),
                enable_tracing: false,
                tracing_level: "warn".to_string(),
                enable_profiling: false,
                profiling_sample_rate: 0.001,
                metrics_collection_interval_ms: 30000,
                enable_health_checks: true,
                health_check_interval_ms: 60000,
            },
            security: SecurityConfig {
                enable_authentication: true,
                api_keys: vec!["supersecretkey".to_string()],
                enable_rate_limiting: true,
                rate_limit_per_connection: 100,
                rate_limit_per_ip: 500,
                rate_limit_window_ms: 1000,
                enable_ddos_protection: false,
                max_payload_size: 32 * 1024,
                enable_audit_logging: false,
            },
        }
    }
    
    /// Load configuration from file or environment variables
    pub fn load_from_env_or_file(file_path: Option<&str>) -> Result<Self, Box<dyn std::error::Error>> {
        // First try environment variable
        if let Ok(profile) = std::env::var("BLIPMQ_PROFILE") {
            return Ok(match profile.to_lowercase().as_str() {
                "low_latency" => Self::low_latency_profile(),
                "high_throughput" => Self::high_throughput_profile(),
                "balanced" => Self::balanced_profile(),
                "resource_efficient" => Self::resource_efficient_profile(),
                _ => Self::balanced_profile(),
            });
        }
        
        // Then try loading from file
        if let Some(path) = file_path {
            if let Ok(content) = std::fs::read_to_string(path) {
                return Ok(toml::from_str(&content)?);
            }
        }
        
        // Default to balanced profile
        Ok(Self::balanced_profile())
    }
    
    /// Auto-tune configuration based on system capabilities
    pub fn auto_tune(&mut self) {
        let cpu_count = num_cpus::get();
        let available_memory = self.get_available_memory_mb();
        
        // Auto-tune worker threads
        self.performance.worker_threads = match self.profile {
            PerformanceProfile::LowLatency => cpu_count,
            PerformanceProfile::HighThroughput => cpu_count * 2,
            PerformanceProfile::Balanced => cpu_count,
            PerformanceProfile::ResourceEfficient => (cpu_count / 2).max(1),
            PerformanceProfile::Custom => self.performance.worker_threads,
        };
        
        // Auto-tune memory limits based on available memory
        if let Some(ref mut limit) = self.memory.memory_limit_mb {
            *limit = (*limit).min(available_memory * 3 / 4); // Use max 75% of available memory
        }
        
        // Auto-tune connection limits
        self.server.max_connections = match available_memory {
            mb if mb < 1024 => 1000,
            mb if mb < 4096 => 5000,
            mb if mb < 16384 => 10000,
            _ => 20000,
        }.min(self.server.max_connections);
    }
    
    fn get_available_memory_mb(&self) -> usize {
        // Simplified - in real implementation, query system memory
        4096 // Assume 4GB available
    }
    
    /// Validate configuration and provide warnings
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        
        if self.server.max_connections > 65536 {
            warnings.push("Very high connection limit may exhaust file descriptors".to_string());
        }
        
        if self.memory.max_message_size > 10 * 1024 * 1024 {
            warnings.push("Large message size may impact performance".to_string());
        }
        
        if self.networking.max_batch_size > 1024 * 1024 {
            warnings.push("Large batch size may increase latency".to_string());
        }
        
        if !self.memory.enable_memory_pooling && self.profile == PerformanceProfile::LowLatency {
            warnings.push("Memory pooling recommended for low-latency profile".to_string());
        }
        
        warnings
    }
    
    /// Generate configuration file template
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }
}

/// Runtime configuration that can be updated without restart
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub rate_limits: std::sync::Arc<std::sync::atomic::AtomicU32>,
    pub max_message_size: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub enable_metrics: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub log_level: std::sync::Arc<parking_lot::RwLock<String>>,
}

impl RuntimeConfig {
    pub fn new(config: &ProductionConfig) -> Self {
        Self {
            rate_limits: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(config.security.rate_limit_per_connection)),
            max_message_size: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(config.memory.max_message_size)),
            enable_metrics: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(config.monitoring.enable_metrics)),
            log_level: std::sync::Arc::new(parking_lot::RwLock::new(config.monitoring.tracing_level.clone())),
        }
    }
}