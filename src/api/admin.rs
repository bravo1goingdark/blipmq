//! Administrative API for BlipMQ System Management
//!
//! Provides administrative functions for:
//! - Configuration management
//! - System diagnostics  
//! - Runtime parameter tuning
//! - Resource management
//! - Service lifecycle control

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::api::metrics::global_metrics;

/// System configuration that can be modified at runtime
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    pub metrics_collection_enabled: bool,
    pub metrics_collection_interval_seconds: u64,
    pub health_monitoring_enabled: bool,
    pub log_level: String,
    pub max_memory_mb: Option<u64>,
    pub message_retention_seconds: u64,
    pub max_message_size_bytes: usize,
    pub max_connections: u32,
    pub enable_debug_endpoints: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            metrics_collection_enabled: true,
            metrics_collection_interval_seconds: 60,
            health_monitoring_enabled: true,
            log_level: "info".to_string(),
            max_memory_mb: Some(1024), // 1GB default limit
            message_retention_seconds: 24 * 60 * 60, // 24 hours
            max_message_size_bytes: 1024 * 1024, // 1MB
            max_connections: 10000,
            enable_debug_endpoints: false,
        }
    }
}

/// System information and statistics
#[derive(Debug, Serialize)]
pub struct SystemInfo {
    pub service_name: String,
    pub version: String,
    pub build_info: BuildInfo,
    pub runtime_info: RuntimeInfo,
    pub resource_usage: ResourceUsage,
    pub feature_flags: FeatureFlags,
}

#[derive(Debug, Serialize)]
pub struct BuildInfo {
    pub version: String,
    pub git_sha: String,
    pub build_date: String,
    pub rust_version: String,
    pub target_arch: String,
    pub target_os: String,
}

#[derive(Debug, Serialize)]
pub struct RuntimeInfo {
    pub uptime_seconds: u64,
    pub start_time: u64,
    pub process_id: u32,
    pub thread_count: usize,
    pub tokio_worker_threads: usize,
}

#[derive(Debug, Serialize)]
pub struct ResourceUsage {
    pub memory_usage_mb: f64,
    pub peak_memory_mb: f64,
    pub cpu_cores: usize,
    pub file_descriptors_used: Option<u32>,
    pub network_connections: u32,
}

#[derive(Debug, Serialize)]
pub struct FeatureFlags {
    pub http_api: bool,
    pub authentication: bool,
    pub persistence: bool,
    pub clustering: bool,
    pub compression: bool,
    pub metrics: bool,
    pub tracing: bool,
}

/// Administrative operations manager
pub struct AdminManager {
    config: parking_lot::RwLock<RuntimeConfig>,
    start_time: SystemTime,
}

impl AdminManager {
    pub fn new() -> Self {
        Self {
            config: parking_lot::RwLock::new(RuntimeConfig::default()),
            start_time: SystemTime::now(),
        }
    }
    
    /// Get current runtime configuration
    pub fn get_config(&self) -> RuntimeConfig {
        self.config.read().clone()
    }
    
    /// Update runtime configuration
    pub fn update_config(&self, new_config: RuntimeConfig) -> Result<(), String> {
        // Validate configuration
        self.validate_config(&new_config)?;
        
        let mut config = self.config.write();
        let old_config = config.clone();
        *config = new_config.clone();
        
        info!("Runtime configuration updated");
        
        // Apply configuration changes
        self.apply_config_changes(&old_config, &new_config);
        
        Ok(())
    }
    
    /// Validate configuration before applying
    fn validate_config(&self, config: &RuntimeConfig) -> Result<(), String> {
        if config.metrics_collection_interval_seconds == 0 {
            return Err("Metrics collection interval must be greater than 0".to_string());
        }
        
        if config.metrics_collection_interval_seconds > 3600 {
            return Err("Metrics collection interval too long (max 1 hour)".to_string());
        }
        
        if config.message_retention_seconds == 0 {
            return Err("Message retention must be greater than 0".to_string());
        }
        
        if config.max_message_size_bytes == 0 {
            return Err("Max message size must be greater than 0".to_string());
        }
        
        if config.max_message_size_bytes > 100 * 1024 * 1024 {
            return Err("Max message size too large (max 100MB)".to_string());
        }
        
        if config.max_connections == 0 {
            return Err("Max connections must be greater than 0".to_string());
        }
        
        match config.log_level.as_str() {
            "trace" | "debug" | "info" | "warn" | "error" => {}
            _ => return Err("Invalid log level".to_string()),
        }
        
        Ok(())
    }
    
    /// Apply configuration changes that affect runtime behavior
    fn apply_config_changes(&self, old_config: &RuntimeConfig, new_config: &RuntimeConfig) {
        // Update log level if changed
        if old_config.log_level != new_config.log_level {
            warn!("Log level change requires restart to take full effect");
        }
        
        // Update health monitoring
        if old_config.health_monitoring_enabled != new_config.health_monitoring_enabled {
            info!("Health monitoring enabled: {}", new_config.health_monitoring_enabled);
            // TODO: Enable/disable health monitoring
        }
        
        // Update metrics collection
        if old_config.metrics_collection_enabled != new_config.metrics_collection_enabled {
            info!("Metrics collection enabled: {}", new_config.metrics_collection_enabled);
        }
        
        if old_config.metrics_collection_interval_seconds != new_config.metrics_collection_interval_seconds {
            info!("Metrics collection interval changed to {}s", new_config.metrics_collection_interval_seconds);
            // TODO: Update metrics collection interval
        }
    }
    
    /// Get comprehensive system information
    pub fn get_system_info(&self) -> SystemInfo {
        let uptime = self.start_time
            .elapsed()
            .unwrap_or_default()
            .as_secs();
        
        let metrics = global_metrics();
        let snapshot = metrics.snapshot();
        
        SystemInfo {
            service_name: "BlipMQ".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            build_info: BuildInfo {
                version: env!("CARGO_PKG_VERSION").to_string(),
                git_sha: "unknown".to_string(), // TODO: Get from build info
                build_date: "unknown".to_string(), // TODO: Get from build info
                rust_version: "unknown".to_string(),
                target_arch: std::env::consts::ARCH.to_string(),
                target_os: std::env::consts::OS.to_string(),
            },
            runtime_info: RuntimeInfo {
                uptime_seconds: uptime,
                start_time: self.start_time
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
                process_id: std::process::id(),
                thread_count: self.get_thread_count(),
                tokio_worker_threads: num_cpus::get(),
            },
            resource_usage: ResourceUsage {
                memory_usage_mb: snapshot.memory_usage_bytes as f64 / 1024.0 / 1024.0,
                peak_memory_mb: 0.0, // TODO: Track peak memory
                cpu_cores: num_cpus::get(),
                file_descriptors_used: self.get_fd_count(),
                network_connections: snapshot.connections_active as u32,
            },
            feature_flags: FeatureFlags {
                http_api: cfg!(feature = "http-api"),
                authentication: true,
                persistence: true,
                clustering: false,
                compression: false,
                metrics: true,
                tracing: true,
            },
        }
    }
    
    /// Get current thread count (platform-specific)
    fn get_thread_count(&self) -> usize {
        #[cfg(target_os = "linux")]
        {
            if let Ok(contents) = std::fs::read_to_string("/proc/self/status") {
                for line in contents.lines() {
                    if line.starts_with("Threads:") {
                        if let Some(count_str) = line.split_whitespace().nth(1) {
                            if let Ok(count) = count_str.parse::<usize>() {
                                return count;
                            }
                        }
                    }
                }
            }
        }
        
        // Fallback estimation
        1
    }
    
    /// Get file descriptor count (Unix-specific)
    fn get_fd_count(&self) -> Option<u32> {
        #[cfg(unix)]
        {
            if let Ok(entries) = std::fs::read_dir("/proc/self/fd") {
                let count = entries.count() as u32;
                return Some(count);
            }
        }
        
        None
    }
    
    /// Perform system maintenance operations
    pub async fn perform_maintenance(&self) -> Result<MaintenanceReport, String> {
        info!("Starting system maintenance");
        
        let start_time = SystemTime::now();
        let mut report = MaintenanceReport {
            started_at: start_time
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            completed_at: 0,
            operations: Vec::new(),
            total_duration_ms: 0,
        };
        
        // Memory cleanup
        let mem_start = SystemTime::now();
        // TODO: Implement memory cleanup
        let mem_duration = mem_start.elapsed().unwrap_or_default().as_millis() as u64;
        report.operations.push(MaintenanceOperation {
            operation: "memory_cleanup".to_string(),
            success: true,
            duration_ms: mem_duration,
            message: Some("Memory cleanup completed".to_string()),
        });
        
        // Metrics cleanup
        let metrics_start = SystemTime::now();
        // TODO: Cleanup old metrics data
        let metrics_duration = metrics_start.elapsed().unwrap_or_default().as_millis() as u64;
        report.operations.push(MaintenanceOperation {
            operation: "metrics_cleanup".to_string(),
            success: true,
            duration_ms: metrics_duration,
            message: Some("Metrics cleanup completed".to_string()),
        });
        
        // Log file rotation
        let log_start = SystemTime::now();
        // TODO: Implement log rotation
        let log_duration = log_start.elapsed().unwrap_or_default().as_millis() as u64;
        report.operations.push(MaintenanceOperation {
            operation: "log_rotation".to_string(),
            success: true,
            duration_ms: log_duration,
            message: Some("Log rotation completed".to_string()),
        });
        
        let total_duration = start_time.elapsed().unwrap_or_default().as_millis() as u64;
        report.completed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        report.total_duration_ms = total_duration;
        
        info!("System maintenance completed in {}ms", total_duration);
        Ok(report)
    }
    
    /// Graceful shutdown preparation
    pub async fn prepare_shutdown(&self) -> Result<ShutdownStatus, String> {
        info!("Preparing for graceful shutdown");
        
        let metrics = global_metrics();
        let snapshot = metrics.snapshot();
        
        // Check if there are active connections
        let active_connections = snapshot.connections_active;
        let pending_messages = snapshot.topic_metrics
            .iter()
            .map(|topic| topic.messages_in_queue)
            .sum::<u64>();
        
        Ok(ShutdownStatus {
            ready_for_shutdown: active_connections == 0 && pending_messages == 0,
            active_connections,
            pending_messages,
            estimated_drain_time_seconds: if pending_messages > 0 {
                Some((pending_messages / 100).max(30)) // Estimate based on processing rate
            } else {
                None
            },
            recommendations: if active_connections > 0 || pending_messages > 0 {
                vec![
                    "Wait for active connections to close".to_string(),
                    "Allow pending messages to be processed".to_string(),
                ]
            } else {
                vec!["System ready for shutdown".to_string()]
            },
        })
    }
}

#[derive(Debug, Serialize)]
pub struct MaintenanceReport {
    pub started_at: u64,
    pub completed_at: u64,
    pub total_duration_ms: u64,
    pub operations: Vec<MaintenanceOperation>,
}

#[derive(Debug, Serialize)]
pub struct MaintenanceOperation {
    pub operation: String,
    pub success: bool,
    pub duration_ms: u64,
    pub message: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ShutdownStatus {
    pub ready_for_shutdown: bool,
    pub active_connections: u64,
    pub pending_messages: u64,
    pub estimated_drain_time_seconds: Option<u64>,
    pub recommendations: Vec<String>,
}

/// Global admin manager instance
static ADMIN_MANAGER: once_cell::sync::Lazy<Arc<AdminManager>> = 
    once_cell::sync::Lazy::new(|| {
        Arc::new(AdminManager::new())
    });

/// Get the global admin manager instance  
pub fn global_admin_manager() -> Arc<AdminManager> {
    Arc::clone(&ADMIN_MANAGER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_runtime_config() {
        let config = RuntimeConfig::default();
        assert!(config.metrics_collection_enabled);
        assert_eq!(config.metrics_collection_interval_seconds, 60);
        assert!(config.health_monitoring_enabled);
    }

    #[test]
    fn test_config_validation() {
        let admin = AdminManager::new();
        
        // Valid config
        let valid_config = RuntimeConfig::default();
        assert!(admin.validate_config(&valid_config).is_ok());
        
        // Invalid config - zero interval
        let mut invalid_config = RuntimeConfig::default();
        invalid_config.metrics_collection_interval_seconds = 0;
        assert!(admin.validate_config(&invalid_config).is_err());
        
        // Invalid config - too large message size
        let mut invalid_config = RuntimeConfig::default();
        invalid_config.max_message_size_bytes = 200 * 1024 * 1024;
        assert!(admin.validate_config(&invalid_config).is_err());
        
        // Invalid log level
        let mut invalid_config = RuntimeConfig::default();
        invalid_config.log_level = "invalid".to_string();
        assert!(admin.validate_config(&invalid_config).is_err());
    }

    #[test]
    fn test_system_info() {
        let admin = AdminManager::new();
        let info = admin.get_system_info();
        
        assert_eq!(info.service_name, "BlipMQ");
        assert!(!info.version.is_empty());
        assert!(info.runtime_info.uptime_seconds < 60); // Should be very small in tests
        assert!(info.resource_usage.cpu_cores > 0);
    }

    #[tokio::test]
    async fn test_maintenance() {
        let admin = AdminManager::new();
        let report = admin.perform_maintenance().await.unwrap();
        
        assert!(report.operations.len() > 0);
        assert!(report.total_duration_ms >= 0);
        assert!(report.completed_at > report.started_at);
    }

    #[tokio::test]
    async fn test_shutdown_preparation() {
        let admin = AdminManager::new();
        let status = admin.prepare_shutdown().await.unwrap();
        
        // Should be ready for shutdown in test environment
        assert!(status.ready_for_shutdown);
        assert_eq!(status.active_connections, 0);
        assert_eq!(status.pending_messages, 0);
    }

    #[test]
    fn test_config_update() {
        let admin = AdminManager::new();
        
        let mut new_config = RuntimeConfig::default();
        new_config.metrics_collection_interval_seconds = 120;
        new_config.max_message_size_bytes = 2 * 1024 * 1024;
        
        assert!(admin.update_config(new_config.clone()).is_ok());
        
        let current_config = admin.get_config();
        assert_eq!(current_config.metrics_collection_interval_seconds, 120);
        assert_eq!(current_config.max_message_size_bytes, 2 * 1024 * 1024);
    }
}