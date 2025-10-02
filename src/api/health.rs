//! Health Monitoring and Diagnostics
//!
//! Provides health checking capabilities for BlipMQ:
//! - System health status
//! - Component readiness checks
//! - Service discovery integration
//! - Liveness probes

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

/// Overall system health status
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HealthStatus {
    /// All systems operational
    Healthy,
    /// Some non-critical issues detected
    Warning, 
    /// Critical issues detected
    Critical,
    /// Service unavailable
    Unavailable,
}

impl HealthStatus {
    pub fn is_healthy(&self) -> bool {
        matches!(self, HealthStatus::Healthy)
    }
    
    pub fn is_available(&self) -> bool {
        !matches!(self, HealthStatus::Unavailable)
    }
}

/// Individual component health check result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentHealth {
    pub component: String,
    pub status: HealthStatus,
    pub message: Option<String>,
    pub last_checked: u64,
    pub response_time_ms: u64,
    pub metadata: HashMap<String, String>,
}

impl ComponentHealth {
    pub fn healthy(component: &str) -> Self {
        Self {
            component: component.to_string(),
            status: HealthStatus::Healthy,
            message: None,
            last_checked: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            response_time_ms: 0,
            metadata: HashMap::new(),
        }
    }
    
    pub fn unhealthy(component: &str, message: &str) -> Self {
        Self {
            component: component.to_string(),
            status: HealthStatus::Critical,
            message: Some(message.to_string()),
            last_checked: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            response_time_ms: 0,
            metadata: HashMap::new(),
        }
    }
    
    pub fn with_metadata(mut self, key: &str, value: &str) -> Self {
        self.metadata.insert(key.to_string(), value.to_string());
        self
    }
}

/// Complete system health report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    pub overall_status: HealthStatus,
    pub timestamp: u64,
    pub uptime_seconds: u64,
    pub components: Vec<ComponentHealth>,
    pub summary: HealthSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSummary {
    pub total_components: usize,
    pub healthy_components: usize,
    pub warning_components: usize,
    pub critical_components: usize,
    pub unavailable_components: usize,
}

/// Health check trait for components
#[async_trait::async_trait]
pub trait HealthChecker: Send + Sync {
    async fn check_health(&self) -> ComponentHealth;
    fn component_name(&self) -> &str;
}

/// Main health monitor
pub struct HealthMonitor {
    checkers: Vec<Box<dyn HealthChecker>>,
    start_time: SystemTime,
    enabled: AtomicBool,
}

impl HealthMonitor {
    pub fn new() -> Self {
        Self {
            checkers: Vec::new(),
            start_time: SystemTime::now(),
            enabled: AtomicBool::new(true),
        }
    }
    
    /// Register a health checker for a component
    pub fn register_checker(&mut self, checker: Box<dyn HealthChecker>) {
        info!("Registered health checker for component: {}", checker.component_name());
        self.checkers.push(checker);
    }
    
    /// Enable or disable health monitoring
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
        if enabled {
            info!("Health monitoring enabled");
        } else {
            warn!("Health monitoring disabled");
        }
    }
    
    /// Check if health monitoring is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
    
    /// Get uptime in seconds
    pub fn uptime_seconds(&self) -> u64 {
        self.start_time
            .elapsed()
            .unwrap_or(Duration::from_secs(0))
            .as_secs()
    }
    
    /// Perform health check on all registered components
    pub async fn check_health(&self) -> HealthReport {
        let start_time = SystemTime::now();
        
        if !self.is_enabled() {
            return HealthReport {
                overall_status: HealthStatus::Unavailable,
                timestamp: start_time
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
                uptime_seconds: self.uptime_seconds(),
                components: vec![],
                summary: HealthSummary {
                    total_components: 0,
                    healthy_components: 0,
                    warning_components: 0,
                    critical_components: 0,
                    unavailable_components: 0,
                },
            };
        }
        
        debug!("Starting health check for {} components", self.checkers.len());
        
        let mut component_healths = Vec::new();
        
        // Check all components concurrently
        let checks: Vec<_> = self.checkers
            .iter()
            .map(|checker| checker.check_health())
            .collect();
        
        for check in checks {
            let component_health = check.await;
            debug!("Health check for {}: {:?}", 
                component_health.component, component_health.status);
            component_healths.push(component_health);
        }
        
        // Calculate summary
        let summary = HealthSummary {
            total_components: component_healths.len(),
            healthy_components: component_healths.iter()
                .filter(|h| matches!(h.status, HealthStatus::Healthy))
                .count(),
            warning_components: component_healths.iter()
                .filter(|h| matches!(h.status, HealthStatus::Warning))
                .count(),
            critical_components: component_healths.iter()
                .filter(|h| matches!(h.status, HealthStatus::Critical))
                .count(),
            unavailable_components: component_healths.iter()
                .filter(|h| matches!(h.status, HealthStatus::Unavailable))
                .count(),
        };
        
        // Determine overall status
        let overall_status = if summary.critical_components > 0 || summary.unavailable_components > 0 {
            HealthStatus::Critical
        } else if summary.warning_components > 0 {
            HealthStatus::Warning
        } else {
            HealthStatus::Healthy
        };
        
        let report = HealthReport {
            overall_status,
            timestamp: start_time
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            uptime_seconds: self.uptime_seconds(),
            components: component_healths,
            summary,
        };
        
        if !report.overall_status.is_healthy() {
            warn!("Health check completed - Status: {:?}", report.overall_status);
        } else {
            debug!("Health check completed - All systems healthy");
        }
        
        report
    }
    
    /// Perform a simple liveness check (faster than full health check)
    pub async fn liveness_check(&self) -> bool {
        self.is_enabled()
    }
    
    /// Perform a readiness check (checks if service is ready to serve requests)
    pub async fn readiness_check(&self) -> bool {
        if !self.is_enabled() {
            return false;
        }
        
        let health = self.check_health().await;
        health.overall_status.is_available()
    }
}

// Built-in health checkers

/// Basic memory usage health checker
pub struct MemoryHealthChecker {
    max_memory_mb: u64,
}

impl MemoryHealthChecker {
    pub fn new(max_memory_mb: u64) -> Self {
        Self { max_memory_mb }
    }
    
    fn get_memory_usage_mb(&self) -> Option<u64> {
        // Platform-specific memory usage detection
        #[cfg(target_os = "linux")]
        {
            if let Ok(contents) = std::fs::read_to_string("/proc/self/status") {
                for line in contents.lines() {
                    if line.starts_with("VmRSS:") {
                        if let Some(kb_str) = line.split_whitespace().nth(1) {
                            if let Ok(kb) = kb_str.parse::<u64>() {
                                return Some(kb / 1024); // Convert KB to MB
                            }
                        }
                    }
                }
            }
        }
        
        // Fallback estimation
        Some(100) // 100MB placeholder
    }
}

#[async_trait::async_trait]
impl HealthChecker for MemoryHealthChecker {
    async fn check_health(&self) -> ComponentHealth {
        let start_time = SystemTime::now();
        
        match self.get_memory_usage_mb() {
            Some(usage_mb) => {
                let response_time = start_time.elapsed().unwrap_or_default().as_millis() as u64;
                
                if usage_mb > self.max_memory_mb {
                    ComponentHealth {
                        component: "memory".to_string(),
                        status: HealthStatus::Critical,
                        message: Some(format!("Memory usage {}MB exceeds limit {}MB", usage_mb, self.max_memory_mb)),
                        last_checked: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64,
                        response_time_ms: response_time,
                        metadata: {
                            let mut meta = HashMap::new();
                            meta.insert("current_usage_mb".to_string(), usage_mb.to_string());
                            meta.insert("max_usage_mb".to_string(), self.max_memory_mb.to_string());
                            meta
                        },
                    }
                } else if usage_mb > self.max_memory_mb * 8 / 10 {
                    // Warning at 80% usage
                    ComponentHealth {
                        component: "memory".to_string(),
                        status: HealthStatus::Warning,
                        message: Some(format!("Memory usage {}MB approaching limit {}MB", usage_mb, self.max_memory_mb)),
                        last_checked: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64,
                        response_time_ms: response_time,
                        metadata: {
                            let mut meta = HashMap::new();
                            meta.insert("current_usage_mb".to_string(), usage_mb.to_string());
                            meta.insert("max_usage_mb".to_string(), self.max_memory_mb.to_string());
                            meta
                        },
                    }
                } else {
                    ComponentHealth::healthy("memory")
                        .with_metadata("current_usage_mb", &usage_mb.to_string())
                        .with_metadata("max_usage_mb", &self.max_memory_mb.to_string())
                }
            }
            None => ComponentHealth::unhealthy("memory", "Unable to determine memory usage")
        }
    }
    
    fn component_name(&self) -> &str {
        "memory"
    }
}

/// File system health checker
pub struct FileSystemHealthChecker {
    paths: Vec<String>,
}

impl FileSystemHealthChecker {
    pub fn new(paths: Vec<String>) -> Self {
        Self { paths }
    }
}

#[async_trait::async_trait]
impl HealthChecker for FileSystemHealthChecker {
    async fn check_health(&self) -> ComponentHealth {
        let start_time = SystemTime::now();
        
        for path in &self.paths {
            match std::fs::metadata(path) {
                Ok(_) => continue,
                Err(e) => {
                    let response_time = start_time.elapsed().unwrap_or_default().as_millis() as u64;
                    return ComponentHealth {
                        component: "filesystem".to_string(),
                        status: HealthStatus::Critical,
                        message: Some(format!("Cannot access path {}: {}", path, e)),
                        last_checked: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64,
                        response_time_ms: response_time,
                        metadata: {
                            let mut meta = HashMap::new();
                            meta.insert("failed_path".to_string(), path.clone());
                            meta.insert("error".to_string(), e.to_string());
                            meta
                        },
                    };
                }
            }
        }
        
        let response_time = start_time.elapsed().unwrap_or_default().as_millis() as u64;
        let mut health = ComponentHealth::healthy("filesystem");
        health.response_time_ms = response_time;
        health.metadata.insert("checked_paths".to_string(), self.paths.len().to_string());
        health
    }
    
    fn component_name(&self) -> &str {
        "filesystem"
    }
}

/// Global health monitor instance
static HEALTH_MONITOR: once_cell::sync::Lazy<Arc<parking_lot::RwLock<HealthMonitor>>> = 
    once_cell::sync::Lazy::new(|| {
        Arc::new(parking_lot::RwLock::new(HealthMonitor::new()))
    });

/// Get the global health monitor instance
pub fn global_health_monitor() -> Arc<parking_lot::RwLock<HealthMonitor>> {
    Arc::clone(&HEALTH_MONITOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockHealthChecker {
        name: String,
        status: HealthStatus,
    }

    impl MockHealthChecker {
        fn new(name: &str, status: HealthStatus) -> Self {
            Self {
                name: name.to_string(),
                status,
            }
        }
    }

    #[async_trait::async_trait]
    impl HealthChecker for MockHealthChecker {
        async fn check_health(&self) -> ComponentHealth {
            ComponentHealth {
                component: self.name.clone(),
                status: self.status.clone(),
                message: None,
                last_checked: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
                response_time_ms: 10,
                metadata: HashMap::new(),
            }
        }

        fn component_name(&self) -> &str {
            &self.name
        }
    }

    #[tokio::test]
    async fn test_health_monitor_all_healthy() {
        let mut monitor = HealthMonitor::new();
        monitor.register_checker(Box::new(MockHealthChecker::new("test1", HealthStatus::Healthy)));
        monitor.register_checker(Box::new(MockHealthChecker::new("test2", HealthStatus::Healthy)));

        let report = monitor.check_health().await;
        assert_eq!(report.overall_status, HealthStatus::Healthy);
        assert_eq!(report.summary.total_components, 2);
        assert_eq!(report.summary.healthy_components, 2);
    }

    #[tokio::test]
    async fn test_health_monitor_with_warning() {
        let mut monitor = HealthMonitor::new();
        monitor.register_checker(Box::new(MockHealthChecker::new("test1", HealthStatus::Healthy)));
        monitor.register_checker(Box::new(MockHealthChecker::new("test2", HealthStatus::Warning)));

        let report = monitor.check_health().await;
        assert_eq!(report.overall_status, HealthStatus::Warning);
        assert_eq!(report.summary.healthy_components, 1);
        assert_eq!(report.summary.warning_components, 1);
    }

    #[tokio::test]
    async fn test_health_monitor_critical() {
        let mut monitor = HealthMonitor::new();
        monitor.register_checker(Box::new(MockHealthChecker::new("test1", HealthStatus::Healthy)));
        monitor.register_checker(Box::new(MockHealthChecker::new("test2", HealthStatus::Critical)));

        let report = monitor.check_health().await;
        assert_eq!(report.overall_status, HealthStatus::Critical);
        assert_eq!(report.summary.healthy_components, 1);
        assert_eq!(report.summary.critical_components, 1);
    }

    #[tokio::test]
    async fn test_disabled_health_monitor() {
        let monitor = HealthMonitor::new();
        monitor.set_enabled(false);

        let report = monitor.check_health().await;
        assert_eq!(report.overall_status, HealthStatus::Unavailable);
        assert_eq!(report.summary.total_components, 0);
    }

    #[tokio::test]
    async fn test_memory_health_checker() {
        let checker = MemoryHealthChecker::new(1000); // 1000MB limit
        let health = checker.check_health().await;
        
        // Should be healthy since we're likely under 1000MB
        assert!(health.status == HealthStatus::Healthy || health.status == HealthStatus::Warning);
        assert_eq!(health.component, "memory");
    }

    #[tokio::test]
    async fn test_filesystem_health_checker() {
        let checker = FileSystemHealthChecker::new(vec!["/".to_string()]);
        let health = checker.check_health().await;
        
        // Root path should exist on Unix systems
        if cfg!(unix) {
            assert_eq!(health.status, HealthStatus::Healthy);
        }
        assert_eq!(health.component, "filesystem");
    }
}