//! BlipMQ REST API and Management Interface
//!
//! Provides HTTP/REST endpoints for:
//! - System monitoring and metrics
//! - Topic and subscription management
//! - User and permission administration
//! - Dead letter queue management
//! - Configuration and health checks

#[cfg(feature = "http-api")]
pub mod rest;

#[cfg(feature = "http-api")]
pub mod handlers;

#[cfg(feature = "http-api")]
pub mod middleware;

// Always available - internal metrics collection
pub mod metrics;
pub mod health;
pub mod admin;

use std::sync::Arc;
use serde::{Deserialize, Serialize};

use crate::core::auth::AuthManager;
use crate::core::dlq::DeadLetterQueue;
use crate::core::timer_wheel::TimerManager;
use crate::core::memory_pool::global_pools;

/// API configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    pub enabled: bool,
    pub bind_addr: String,
    pub bind_port: u16,
    pub enable_cors: bool,
    pub enable_tracing: bool,
    pub max_request_size: usize,
    pub request_timeout_ms: u64,
    pub require_authentication: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: false, // Disabled by default for security
            bind_addr: "127.0.0.1".to_string(),
            bind_port: 8080,
            enable_cors: true,
            enable_tracing: true,
            max_request_size: 1024 * 1024, // 1MB
            request_timeout_ms: 30_000,     // 30 seconds
            require_authentication: true,
        }
    }
}

/// Shared application state for API handlers
#[derive(Clone)]
pub struct ApiState {
    pub auth_manager: Arc<AuthManager>,
    pub timer_manager: Arc<TimerManager>,
    pub dlq: Arc<DeadLetterQueue>,
    pub config: ApiConfig,
}

/// Standard API response wrapper
#[derive(Debug, Serialize, Deserialize)]
pub struct ApiResponse<T> {
    pub success: bool,
    pub data: Option<T>,
    pub error: Option<String>,
    pub timestamp: u64,
}

impl<T> ApiResponse<T> {
    pub fn success(data: T) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        }
    }

    pub fn error(message: String) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(message),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        }
    }
}