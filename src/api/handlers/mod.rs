//! HTTP request handlers for BlipMQ API
//!
//! Provides RESTful HTTP endpoints for:
//! - Publishing and consuming messages
//! - Topic management
//! - Authentication
//! - System administration
//! - Monitoring and metrics

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;
use tracing::error;

// Module exports
pub mod metrics;

/// API error types
#[derive(Debug, Error)]
pub enum ApiError {
    #[error("Bad request: {0}")]
    BadRequest(String),
    
    #[error("Unauthorized: {0}")]
    Unauthorized(String),
    
    #[error("Forbidden: {0}")]
    Forbidden(String),
    
    #[error("Not found: {0}")]
    NotFound(String),
    
    #[error("Conflict: {0}")]
    Conflict(String),
    
    #[error("Unprocessable entity: {0}")]
    UnprocessableEntity(String),
    
    #[error("Too many requests: {0}")]
    TooManyRequests(String),
    
    #[error("Internal server error: {0}")]
    InternalServerError(String),
    
    #[error("Service unavailable: {0}")]
    ServiceUnavailable(String),
    
    #[error("Timeout: {0}")]
    Timeout(String),
}

impl ApiError {
    fn status_code(&self) -> StatusCode {
        match self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden(_) => StatusCode::FORBIDDEN,
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::UnprocessableEntity(_) => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::TooManyRequests(_) => StatusCode::TOO_MANY_REQUESTS,
            ApiError::InternalServerError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Timeout(_) => StatusCode::REQUEST_TIMEOUT,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        error!("API error: {}", self);
        
        let status = self.status_code();
        let body = Json(ApiResponse::<()>::error(self.to_string()));
        
        (status, body).into_response()
    }
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

/// Pagination parameters
#[derive(Debug, Deserialize)]
pub struct PaginationQuery {
    pub page: Option<u32>,
    pub per_page: Option<u32>,
    pub sort: Option<String>,
    pub order: Option<String>, // "asc" or "desc"
}

impl Default for PaginationQuery {
    fn default() -> Self {
        Self {
            page: Some(1),
            per_page: Some(50),
            sort: None,
            order: Some("desc".to_string()),
        }
    }
}

/// Paginated response wrapper
#[derive(Debug, Serialize)]
pub struct PaginatedResponse<T> {
    pub items: Vec<T>,
    pub total_items: u64,
    pub total_pages: u32,
    pub current_page: u32,
    pub per_page: u32,
    pub has_next_page: bool,
    pub has_prev_page: bool,
}

impl<T> PaginatedResponse<T> {
    pub fn new(
        items: Vec<T>,
        total_items: u64,
        page: u32,
        per_page: u32,
    ) -> Self {
        let total_pages = ((total_items as f64) / (per_page as f64)).ceil() as u32;
        let total_pages = total_pages.max(1);
        
        Self {
            items,
            total_items,
            total_pages,
            current_page: page,
            per_page,
            has_next_page: page < total_pages,
            has_prev_page: page > 1,
        }
    }
}

/// Generic filter parameters for list endpoints
#[derive(Debug, Deserialize)]
pub struct FilterQuery {
    pub filter: Option<String>,
    pub search: Option<String>,
    pub status: Option<String>,
    pub created_after: Option<u64>,
    pub created_before: Option<u64>,
}

/// Request validation utilities
pub mod validation {
    use super::ApiError;

    pub fn validate_topic_name(name: &str) -> Result<(), ApiError> {
        if name.is_empty() {
            return Err(ApiError::BadRequest("Topic name cannot be empty".to_string()));
        }
        
        if name.len() > 255 {
            return Err(ApiError::BadRequest("Topic name too long (max 255 characters)".to_string()));
        }
        
        if name.contains(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '.') {
            return Err(ApiError::BadRequest("Topic name contains invalid characters".to_string()));
        }
        
        Ok(())
    }
    
    pub fn validate_message_size(size: usize, max_size: usize) -> Result<(), ApiError> {
        if size > max_size {
            return Err(ApiError::UnprocessableEntity(
                format!("Message size {} exceeds maximum of {}", size, max_size)
            ));
        }
        Ok(())
    }
    
    pub fn validate_positive_number(value: i64, field_name: &str) -> Result<(), ApiError> {
        if value <= 0 {
            return Err(ApiError::BadRequest(
                format!("{} must be a positive number", field_name)
            ));
        }
        Ok(())
    }
}