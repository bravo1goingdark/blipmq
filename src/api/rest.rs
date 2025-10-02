//! REST API Server and Routing Configuration
//!
//! Sets up the HTTP server with comprehensive REST endpoints for:
//! - System monitoring and metrics
//! - Health checks and diagnostics  
//! - Topic and subscription management (future)
//! - Authentication and authorization (future)

use axum::{
    extract::DefaultBodyLimit,
    http::{
        header::{AUTHORIZATION, CONTENT_TYPE},
        Method, StatusCode,
    },
    middleware,
    response::Json,
    routing::{get, post},
    serve,
    Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
    timeout::TimeoutLayer,
};
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::api::{ApiConfig, ApiResponse};
use crate::api::handlers;
use crate::api::handlers::metrics;

/// Builds the complete REST API router with all endpoints
pub fn create_router() -> Router<crate::api::AppState> {
    // Health and status endpoints
    let health_routes = Router::new()
        .route("/health", get(metrics::health_check))
        .route("/ping", get(ping_handler))
        .route("/version", get(version_handler));
    
    // Metrics and monitoring endpoints
    let metrics_routes = Router::new()
        .route("/metrics", get(metrics::get_metrics))
        .route("/metrics/summary", get(metrics::get_system_summary))
        .route("/metrics/history", get(metrics::get_historical_metrics))
        .route("/metrics/topics/:topic_name", get(metrics::get_topic_metrics))
        .route("/metrics/alerts", get(metrics::get_alerts));
    
    // API v1 routes
    let v1_routes = Router::new()
        .nest("/health", health_routes)
        .nest("/metrics", metrics_routes)
        // TODO: Add more route groups here
        // .nest("/topics", topic_routes)
        // .nest("/auth", auth_routes)
        // .nest("/admin", admin_routes)
        ;
    
    // Main router with versioning and error handling
    Router::new()
        .nest("/api/v1", v1_routes)
        .route("/", get(root_handler))
        .fallback(not_found_handler)
}

/// Configures middleware stack for the API server
pub fn create_middleware_stack(config: &ApiConfig) -> ServiceBuilder<tower::layer::util::Stack<
    tower::layer::util::Stack<
        tower::layer::util::Stack<
            tower::layer::util::Stack<
                tower::layer::util::Identity,
                DefaultBodyLimit,
            >,
            TimeoutLayer,
        >,
        TraceLayer,
    >,
    CorsLayer,
>> {
    ServiceBuilder::new()
        // Request size limits
        .layer(DefaultBodyLimit::max(config.max_request_size))
        // Request timeouts
        .layer(TimeoutLayer::new(std::time::Duration::from_millis(config.request_timeout_ms)))
        // Request tracing (if enabled)
        .layer(if config.enable_tracing {
            TraceLayer::new_for_http()
        } else {
            TraceLayer::new_for_http().make_span_with(|_| tracing::info_span!(""))
        })
        // CORS (if enabled)
        .layer(if config.enable_cors {
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE, Method::PATCH])
                .allow_headers([AUTHORIZATION, CONTENT_TYPE])
                .allow_origin(Any)
        } else {
            CorsLayer::new()
        })
}

/// Starts the HTTP REST API server
pub async fn start_server(
    app_state: crate::api::AppState,
    config: ApiConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !config.enabled {
        warn!("REST API server is disabled in configuration");
        return Ok(());
    }
    
    let app = create_router()
        .layer(create_middleware_stack(&config))
        .with_state(app_state.clone());
    
    let addr: SocketAddr = format!("{}:{}", config.bind_addr, config.bind_port)
        .parse()
        .map_err(|e| format!("Invalid bind address: {}", e))?;
    
    info!("Starting REST API server on {}", addr);
    info!("API endpoints available:");
    info!("  - Health: http://{}/api/v1/health", addr);
    info!("  - Metrics: http://{}/api/v1/metrics", addr);
    info!("  - System Summary: http://{}/api/v1/metrics/summary", addr);
    info!("  - Historical Data: http://{}/api/v1/metrics/history", addr);
    info!("  - Topic Metrics: http://{}/api/v1/metrics/topics/{{topic_name}}", addr);
    info!("  - Alerts: http://{}/api/v1/metrics/alerts", addr);
    
    let listener = TcpListener::bind(addr).await?;
    serve(listener, app).await?;
    
    Ok(())
}

// Basic endpoint handlers

async fn root_handler() -> Json<ApiResponse<serde_json::Value>> {
    Json(ApiResponse::success(serde_json::json!({
        "service": "BlipMQ",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "High-performance message queue with advanced features",
        "api_version": "v1",
        "endpoints": {
            "health": "/api/v1/health",
            "metrics": "/api/v1/metrics",
            "docs": "https://github.com/your-org/blipmq/docs"
        }
    })))
}

async fn ping_handler() -> Json<ApiResponse<serde_json::Value>> {
    Json(ApiResponse::success(serde_json::json!({
        "message": "pong",
        "timestamp": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    })))
}

async fn version_handler() -> Json<ApiResponse<serde_json::Value>> {
    Json(ApiResponse::success(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "build_date": env!("VERGEN_BUILD_DATE"),
        "git_sha": env!("VERGEN_GIT_SHA"),
        "features": {
            "http_api": cfg!(feature = "http-api"),
            "auth": true,
            "metrics": true,
            "persistence": true,
            "clustering": false, // TODO: Update when implemented
        }
    })))
}

async fn not_found_handler() -> (StatusCode, Json<ApiResponse<()>>) {
    (
        StatusCode::NOT_FOUND,
        Json(ApiResponse::error("Endpoint not found".to_string())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum_test::TestServer;
    use crate::api::AppState;
    use crate::core::auth::AuthManager;
    use crate::core::dlq::DeadLetterQueue;
    use crate::core::timer_wheel::TimerManager;

    fn create_test_app_state() -> AppState {
        AppState {
            auth_manager: Arc::new(AuthManager::new()),
            timer_manager: Arc::new(TimerManager::new(1000)),
            dlq: Arc::new(DeadLetterQueue::new()),
            config: ApiConfig::default(),
        }
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let app_state = create_test_app_state();
        let app = create_router().with_state(app_state);
        let server = TestServer::new(app).unwrap();

        let response = server.get("/api/v1/health").await;
        response.assert_status_ok();
        
        let body: ApiResponse<serde_json::Value> = response.json();
        assert!(body.success);
        assert!(body.data.is_some());
    }

    #[tokio::test]
    async fn test_metrics_endpoint() {
        let app_state = create_test_app_state();
        let app = create_router().with_state(app_state);
        let server = TestServer::new(app).unwrap();

        let response = server.get("/api/v1/metrics").await;
        response.assert_status_ok();
        
        let body: ApiResponse<serde_json::Value> = response.json();
        assert!(body.success);
        assert!(body.data.is_some());
    }

    #[tokio::test]
    async fn test_ping_endpoint() {
        let app_state = create_test_app_state();
        let app = create_router().with_state(app_state);
        let server = TestServer::new(app).unwrap();

        let response = server.get("/api/v1/health/ping").await;
        response.assert_status_ok();
        
        let body: ApiResponse<serde_json::Value> = response.json();
        assert!(body.success);
        assert_eq!(body.data.unwrap()["message"], "pong");
    }

    #[tokio::test]
    async fn test_not_found() {
        let app_state = create_test_app_state();
        let app = create_router().with_state(app_state);
        let server = TestServer::new(app).unwrap();

        let response = server.get("/nonexistent").await;
        response.assert_status(StatusCode::NOT_FOUND);
        
        let body: ApiResponse<()> = response.json();
        assert!(!body.success);
        assert!(body.error.is_some());
    }
}