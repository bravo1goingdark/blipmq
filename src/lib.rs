//! BlipMQ – A lightweight, high-throughput message broker written in Rust.
//!
//! This crate exports:
//!  * `core`   – low-level message, queue, topic logic with performance optimizations
//!  * `broker` – TCP server-side engine with production-grade features  
//!  * `config` – TOML-driven runtime configuration
//!
//! ## Production Features
//!
//! BlipMQ includes advanced production-grade optimizations:
//! - Memory pooling for reduced allocations
//! - Vectored I/O for high-throughput batching  
//! - Connection management with rate limiting
//! - Performance monitoring and metrics
//! - Graceful shutdown handling
//! - Multiple performance profiles (Low Latency, High Throughput, etc.)
//!
//! ## Performance Profiles
//!
//! Choose a profile that matches your use case:
//! - **Low Latency**: Financial trading, gaming (sub-millisecond)
//! - **High Throughput**: Analytics, data processing (millions msg/s)
//! - **Balanced**: General production use (default)
//! - **Resource Efficient**: Cloud/container deployments
//!
//! ## Quick Start
//!
//! ```rust
//! use blipmq::{start_optimized_broker, ProductionConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Use a production-optimized profile
//!     let config = ProductionConfig::balanced_profile();
//!     start_optimized_broker(config).await?;
//!     Ok(())
//! }
//! ```
//!
//! Downstream applications can embed the broker engine (`start_broker`) or
//! build their own binaries on top of the library.

// ───────────────────────────────────────────────────────────
// Public modules
// ───────────────────────────────────────────────────────────
pub mod broker;
pub mod config;
pub mod core;
pub mod logging;

// ───────────────────────────────────────────────────────────
// Re-exports - Legacy API
// ───────────────────────────────────────────────────────────
pub use broker::engine::serve as start_broker;
pub use config::{load_config, Config};

// ───────────────────────────────────────────────────────────
// Re-exports - Production API
// ───────────────────────────────────────────────────────────
pub use broker::engine::optimized_server::serve_optimized as start_optimized_broker;
pub use core::advanced_config::ProductionConfig;
pub use core::monitoring::{start_monitoring, MetricsCollector, HealthStatus};
pub use core::performance::{PerformanceMetrics, PERFORMANCE_METRICS};
pub use core::graceful_shutdown::ShutdownCoordinator;

// ───────────────────────────────────────────────────────────
// Production utilities
// ───────────────────────────────────────────────────────────
pub mod prelude {
    //! Commonly used imports for production deployments
    
    pub use crate::core::{
        advanced_config::{ProductionConfig, PerformanceProfile},
        connection_manager::{ConnectionManager, ConnectionLimits},
        memory_pool::{acquire_small_buffer, acquire_medium_buffer, acquire_large_buffer},
        performance::{LatencyTimer, PERFORMANCE_METRICS},
        monitoring::{HealthStatus, MetricsCollector},
        graceful_shutdown::ShutdownCoordinator,
    };
    
    pub use crate::{
        start_optimized_broker,
        start_monitoring,
    };
}

/// High-level production broker builder
pub struct BrokerBuilder {
    config: ProductionConfig,
}

impl Default for BrokerBuilder {
    fn default() -> Self {
        Self {
            config: ProductionConfig::balanced_profile(),
        }
    }
}

impl BrokerBuilder {
    /// Create a new broker builder with default balanced profile
    pub fn new() -> Self {
        Self::default()
    }
    
    /// Use a specific performance profile
    pub fn with_profile(mut self, profile: crate::core::advanced_config::PerformanceProfile) -> Self {
        self.config = match profile {
            crate::core::advanced_config::PerformanceProfile::LowLatency => ProductionConfig::low_latency_profile(),
            crate::core::advanced_config::PerformanceProfile::HighThroughput => ProductionConfig::high_throughput_profile(),
            crate::core::advanced_config::PerformanceProfile::Balanced => ProductionConfig::balanced_profile(),
            crate::core::advanced_config::PerformanceProfile::ResourceEfficient => ProductionConfig::resource_efficient_profile(),
            crate::core::advanced_config::PerformanceProfile::Custom => self.config, // Keep existing
        };
        self
    }
    
    /// Set bind address
    pub fn bind_addr(mut self, addr: impl Into<String>) -> Self {
        self.config.server.bind_addr = addr.into();
        self
    }
    
    /// Set maximum connections
    pub fn max_connections(mut self, max: usize) -> Self {
        self.config.server.max_connections = max;
        self
    }
    
    /// Enable/disable memory pooling
    pub fn memory_pooling(mut self, enabled: bool) -> Self {
        self.config.memory.enable_memory_pooling = enabled;
        self
    }
    
    /// Enable/disable vectored I/O
    pub fn vectored_io(mut self, enabled: bool) -> Self {
        self.config.networking.enable_vectored_io = enabled;
        self
    }
    
    /// Enable/disable monitoring
    pub fn monitoring(mut self, enabled: bool) -> Self {
        self.config.monitoring.enable_metrics = enabled;
        self
    }
    
    /// Load configuration from file
    pub fn load_config(mut self, path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        self.config = ProductionConfig::load_from_env_or_file(Some(path))?;
        Ok(self)
    }
    
    /// Auto-tune configuration based on system
    pub fn auto_tune(mut self) -> Self {
        self.config.auto_tune();
        self
    }
    
    /// Build and start the optimized broker
    pub async fn start(mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Auto-tune if not already done
        self.config.auto_tune();
        
        // Validate configuration
        let warnings = self.config.validate();
        for warning in warnings {
            tracing::warn!("Configuration: {}", warning);
        }
        
        // Start monitoring if enabled
        if self.config.monitoring.enable_metrics {
            let metrics_addr = self.config.monitoring.metrics_bind_addr.clone();
            tokio::spawn(async move {
                if let Err(e) = start_monitoring(metrics_addr).await {
                    tracing::error!("Failed to start monitoring: {}", e);
                }
            });
        }
        
        // Create and start optimized server
        let server = crate::broker::engine::optimized_server::OptimizedServer::new(self.config);
        server.serve().await.map_err(Into::into)
    }
    
    /// Get the current configuration (for inspection/testing)
    pub fn config(&self) -> &ProductionConfig {
        &self.config
    }
}

/// Convenience function to start broker with default balanced profile
pub async fn start_default_broker() -> Result<(), Box<dyn std::error::Error>> {
    BrokerBuilder::new()
        .auto_tune()
        .start()
        .await
}

/// Convenience function to start broker with low latency profile  
pub async fn start_low_latency_broker(bind_addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    BrokerBuilder::new()
        .with_profile(crate::core::advanced_config::PerformanceProfile::LowLatency)
        .bind_addr(bind_addr)
        .auto_tune()
        .start()
        .await
}

/// Convenience function to start broker with high throughput profile
pub async fn start_high_throughput_broker(bind_addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    BrokerBuilder::new()
        .with_profile(crate::core::advanced_config::PerformanceProfile::HighThroughput)
        .bind_addr(bind_addr)
        .auto_tune()
        .start()
        .await
}

// NOTE:
// The CLI (`blipmq-cli`) is now a standalone binary under `src/bin/`,
// so it's no longer re-exported from this library.