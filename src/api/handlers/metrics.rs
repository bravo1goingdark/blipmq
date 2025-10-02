//! HTTP REST handlers for metrics and monitoring endpoints
//!
//! Provides comprehensive monitoring and observability through HTTP API:
//! - Current system metrics
//! - Historical metrics data  
//! - Per-topic metrics
//! - Health checks
//! - Performance analytics

use axum::{
    extract::{Query, Path, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::api::handlers::{ApiError, ApiResponse};
use crate::api::metrics::{global_metrics, MetricsSnapshot, TopicMetricsSnapshot};
use crate::api::AppState;

/// Query parameters for metrics endpoints
#[derive(Debug, Deserialize)]
pub struct MetricsQuery {
    /// Number of historical data points to return
    pub limit: Option<usize>,
    /// Include per-topic metrics
    pub include_topics: Option<bool>,
    /// Time range for historical data (in minutes)
    pub time_range_minutes: Option<u64>,
}

/// Health check response
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub timestamp: u64,
    pub uptime_seconds: u64,
    pub version: String,
    pub memory_usage_mb: f64,
    pub active_connections: u64,
    pub active_topics: usize,
    pub total_messages: u64,
}

/// System summary response
#[derive(Debug, Serialize)]
pub struct SystemSummary {
    pub messages: MessageSummary,
    pub connections: ConnectionSummary,
    pub topics: TopicSummary,
    pub performance: PerformanceSummary,
    pub resources: ResourceSummary,
}

#[derive(Debug, Serialize)]
pub struct MessageSummary {
    pub published: u64,
    pub delivered: u64,
    pub acknowledged: u64,
    pub failed: u64,
    pub dropped_ttl: u64,
    pub dropped_queue_full: u64,
    pub success_rate: f64,
}

#[derive(Debug, Serialize)]
pub struct ConnectionSummary {
    pub active: u64,
    pub total: u64,
    pub failed: u64,
    pub success_rate: f64,
}

#[derive(Debug, Serialize)]
pub struct TopicSummary {
    pub total_topics: u64,
    pub active_subscribers: u64,
    pub avg_messages_per_topic: f64,
    pub most_active_topic: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PerformanceSummary {
    pub publish_latency_ms: f64,
    pub delivery_latency_ms: f64,
    pub throughput_msg_per_sec: f64,
    pub throughput_bytes_per_sec: f64,
}

#[derive(Debug, Serialize)]
pub struct ResourceSummary {
    pub memory_usage_mb: f64,
    pub disk_usage_mb: f64,
    pub cpu_usage_percent: f64,
}

/// Historical trends response
#[derive(Debug, Serialize)]
pub struct HistoricalTrends {
    pub data_points: Vec<MetricsSnapshot>,
    pub trends: TrendsAnalysis,
}

#[derive(Debug, Serialize)]
pub struct TrendsAnalysis {
    pub message_growth_rate: f64,
    pub latency_trend: String, // "improving", "degrading", "stable"
    pub peak_hours: Vec<u8>, // Hours of day with highest activity
    pub error_rate_trend: f64,
}

/// Topic-specific metrics response
#[derive(Debug, Serialize)]
pub struct TopicMetricsResponse {
    pub topic_name: String,
    pub metrics: TopicMetricsSnapshot,
    pub subscribers: Vec<SubscriberInfo>,
    pub message_rates: MessageRates,
}

#[derive(Debug, Serialize)]
pub struct SubscriberInfo {
    pub subscriber_id: String,
    pub connected_at: u64,
    pub messages_delivered: u64,
    pub last_activity: u64,
}

#[derive(Debug, Serialize)]
pub struct MessageRates {
    pub publish_rate_per_min: f64,
    pub delivery_rate_per_min: f64,
    pub queue_depth: u64,
    pub average_message_size_bytes: f64,
}

/// Alerting and thresholds response
#[derive(Debug, Serialize)]
pub struct AlertsResponse {
    pub active_alerts: Vec<Alert>,
    pub thresholds: SystemThresholds,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Alert {
    pub alert_id: String,
    pub severity: String, // "warning", "critical"
    pub message: String,
    pub metric: String,
    pub threshold: f64,
    pub current_value: f64,
    pub triggered_at: u64,
}

#[derive(Debug, Serialize)]
pub struct SystemThresholds {
    pub max_memory_usage_percent: f64,
    pub max_cpu_usage_percent: f64,
    pub max_latency_ms: f64,
    pub min_success_rate_percent: f64,
    pub max_queue_depth: u64,
}

/// Health check endpoint
pub async fn health_check(State(_app_state): State<AppState>) -> Result<Json<ApiResponse<HealthResponse>>, ApiError> {
    let metrics = global_metrics();
    let snapshot = metrics.snapshot();
    
    let health = HealthResponse {
        status: "healthy".to_string(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
        uptime_seconds: 0, // TODO: Track actual uptime
        version: env!("CARGO_PKG_VERSION").to_string(),
        memory_usage_mb: snapshot.memory_usage_bytes as f64 / 1024.0 / 1024.0,
        active_connections: snapshot.connections_active,
        active_topics: snapshot.topic_metrics.len(),
        total_messages: snapshot.messages_published,
    };
    
    Ok(Json(ApiResponse::success(health)))
}

/// Get current system metrics
pub async fn get_metrics(
    Query(query): Query<MetricsQuery>,
    State(_app_state): State<AppState>,
) -> Result<Json<ApiResponse<MetricsSnapshot>>, ApiError> {
    let metrics = global_metrics();
    let mut snapshot = metrics.snapshot();
    
    // Filter topic metrics if requested
    if !query.include_topics.unwrap_or(true) {
        snapshot.topic_metrics.clear();
    }
    
    Ok(Json(ApiResponse::success(snapshot)))
}

/// Get system summary with key performance indicators
pub async fn get_system_summary(
    State(_app_state): State<AppState>,
) -> Result<Json<ApiResponse<SystemSummary>>, ApiError> {
    let metrics = global_metrics();
    let snapshot = metrics.snapshot();
    
    // Calculate success rates
    let message_success_rate = if snapshot.messages_published > 0 {
        (snapshot.messages_delivered as f64 / snapshot.messages_published as f64) * 100.0
    } else {
        100.0
    };
    
    let connection_success_rate = if snapshot.connections_total > 0 {
        ((snapshot.connections_total - snapshot.connections_failed) as f64 / snapshot.connections_total as f64) * 100.0
    } else {
        100.0
    };
    
    // Find most active topic
    let most_active_topic = snapshot.topic_metrics
        .iter()
        .max_by_key(|topic| topic.messages_published)
        .map(|topic| topic.topic_name.clone());
    
    // Calculate average messages per topic
    let avg_messages_per_topic = if !snapshot.topic_metrics.is_empty() {
        snapshot.topic_metrics.iter()
            .map(|topic| topic.messages_published)
            .sum::<u64>() as f64 / snapshot.topic_metrics.len() as f64
    } else {
        0.0
    };
    
    let summary = SystemSummary {
        messages: MessageSummary {
            published: snapshot.messages_published,
            delivered: snapshot.messages_delivered,
            acknowledged: snapshot.messages_acknowledged,
            failed: snapshot.messages_failed,
            dropped_ttl: snapshot.messages_dropped_ttl,
            dropped_queue_full: snapshot.messages_dropped_queue_full,
            success_rate: message_success_rate,
        },
        connections: ConnectionSummary {
            active: snapshot.connections_active,
            total: snapshot.connections_total,
            failed: snapshot.connections_failed,
            success_rate: connection_success_rate,
        },
        topics: TopicSummary {
            total_topics: snapshot.topics_created,
            active_subscribers: snapshot.subscribers_active,
            avg_messages_per_topic,
            most_active_topic,
        },
        performance: PerformanceSummary {
            publish_latency_ms: snapshot.publish_latency.average_ms,
            delivery_latency_ms: snapshot.delivery_latency.average_ms,
            throughput_msg_per_sec: 0.0, // TODO: Calculate from rate
            throughput_bytes_per_sec: 0.0, // TODO: Calculate from rate
        },
        resources: ResourceSummary {
            memory_usage_mb: snapshot.memory_usage_bytes as f64 / 1024.0 / 1024.0,
            disk_usage_mb: snapshot.disk_usage_bytes as f64 / 1024.0 / 1024.0,
            cpu_usage_percent: snapshot.cpu_usage_percent as f64,
        },
    };
    
    Ok(Json(ApiResponse::success(summary)))
}

/// Get historical metrics with trend analysis
pub async fn get_historical_metrics(
    Query(query): Query<MetricsQuery>,
    State(_app_state): State<AppState>,
) -> Result<Json<ApiResponse<HistoricalTrends>>, ApiError> {
    let metrics = global_metrics();
    let history = metrics.get_history(query.limit);
    
    if history.is_empty() {
        return Ok(Json(ApiResponse::success(HistoricalTrends {
            data_points: vec![],
            trends: TrendsAnalysis {
                message_growth_rate: 0.0,
                latency_trend: "stable".to_string(),
                peak_hours: vec![],
                error_rate_trend: 0.0,
            },
        })));
    }
    
    // Analyze trends
    let trends = analyze_trends(&history);
    
    let response = HistoricalTrends {
        data_points: history,
        trends,
    };
    
    Ok(Json(ApiResponse::success(response)))
}

/// Get metrics for a specific topic
pub async fn get_topic_metrics(
    Path(topic_name): Path<String>,
    State(_app_state): State<AppState>,
) -> Result<Json<ApiResponse<TopicMetricsResponse>>, ApiError> {
    let metrics = global_metrics();
    let snapshot = metrics.snapshot();
    
    let topic_metrics = snapshot.topic_metrics
        .iter()
        .find(|t| t.topic_name == topic_name)
        .ok_or_else(|| ApiError::NotFound(format!("Topic '{}' not found", topic_name)))?;
    
    // Mock subscriber data (in production, this would come from the broker)
    let subscribers = vec![
        SubscriberInfo {
            subscriber_id: "sub-1".to_string(),
            connected_at: topic_metrics.created_at,
            messages_delivered: topic_metrics.messages_delivered / 2,
            last_activity: topic_metrics.last_message_at,
        },
    ];
    
    let message_rates = MessageRates {
        publish_rate_per_min: 0.0, // TODO: Calculate actual rates
        delivery_rate_per_min: 0.0,
        queue_depth: topic_metrics.messages_in_queue,
        average_message_size_bytes: if topic_metrics.messages_published > 0 {
            topic_metrics.bytes_published as f64 / topic_metrics.messages_published as f64
        } else {
            0.0
        },
    };
    
    let response = TopicMetricsResponse {
        topic_name: topic_metrics.topic_name.clone(),
        metrics: topic_metrics.clone(),
        subscribers,
        message_rates,
    };
    
    Ok(Json(ApiResponse::success(response)))
}

/// Get system alerts and recommendations
pub async fn get_alerts(
    State(_app_state): State<AppState>,
) -> Result<Json<ApiResponse<AlertsResponse>>, ApiError> {
    let metrics = global_metrics();
    let snapshot = metrics.snapshot();
    
    let thresholds = SystemThresholds {
        max_memory_usage_percent: 80.0,
        max_cpu_usage_percent: 90.0,
        max_latency_ms: 1000.0,
        min_success_rate_percent: 95.0,
        max_queue_depth: 10000,
    };
    
    let mut alerts = Vec::new();
    let mut recommendations = Vec::new();
    
    // Check memory usage
    let memory_usage_percent = (snapshot.memory_usage_bytes as f64 / (1024.0 * 1024.0 * 1024.0)) * 100.0; // Assuming 1GB max
    if memory_usage_percent > thresholds.max_memory_usage_percent {
        alerts.push(Alert {
            alert_id: "mem-001".to_string(),
            severity: if memory_usage_percent > 95.0 { "critical" } else { "warning" }.to_string(),
            message: "High memory usage detected".to_string(),
            metric: "memory_usage_percent".to_string(),
            threshold: thresholds.max_memory_usage_percent,
            current_value: memory_usage_percent,
            triggered_at: snapshot.timestamp,
        });
        recommendations.push("Consider increasing memory limits or optimizing message storage".to_string());
    }
    
    // Check latency
    if snapshot.delivery_latency.average_ms > thresholds.max_latency_ms {
        alerts.push(Alert {
            alert_id: "lat-001".to_string(),
            severity: "warning".to_string(),
            message: "High delivery latency detected".to_string(),
            metric: "delivery_latency_ms".to_string(),
            threshold: thresholds.max_latency_ms,
            current_value: snapshot.delivery_latency.average_ms,
            triggered_at: snapshot.timestamp,
        });
        recommendations.push("Check network conditions and consider message batching optimizations".to_string());
    }
    
    // Check success rate
    let success_rate = if snapshot.messages_published > 0 {
        (snapshot.messages_delivered as f64 / snapshot.messages_published as f64) * 100.0
    } else {
        100.0
    };
    
    if success_rate < thresholds.min_success_rate_percent {
        alerts.push(Alert {
            alert_id: "suc-001".to_string(),
            severity: "critical".to_string(),
            message: "Low message success rate detected".to_string(),
            metric: "success_rate_percent".to_string(),
            threshold: thresholds.min_success_rate_percent,
            current_value: success_rate,
            triggered_at: snapshot.timestamp,
        });
        recommendations.push("Investigate message delivery failures and check subscriber health".to_string());
    }
    
    let response = AlertsResponse {
        active_alerts: alerts,
        thresholds,
        recommendations,
    };
    
    Ok(Json(ApiResponse::success(response)))
}

/// Analyze trends from historical data
fn analyze_trends(history: &[MetricsSnapshot]) -> TrendsAnalysis {
    if history.len() < 2 {
        return TrendsAnalysis {
            message_growth_rate: 0.0,
            latency_trend: "stable".to_string(),
            peak_hours: vec![],
            error_rate_trend: 0.0,
        };
    }
    
    let first = &history[0];
    let last = &history[history.len() - 1];
    
    // Calculate message growth rate
    let time_diff_hours = (last.timestamp - first.timestamp) as f64 / (1000.0 * 60.0 * 60.0);
    let message_growth_rate = if time_diff_hours > 0.0 && first.messages_published > 0 {
        ((last.messages_published as f64 - first.messages_published as f64) / first.messages_published as f64) / time_diff_hours * 100.0
    } else {
        0.0
    };
    
    // Analyze latency trend
    let avg_early_latency: f64 = history.iter()
        .take(history.len() / 2)
        .map(|h| h.delivery_latency.average_ms)
        .sum::<f64>() / (history.len() / 2) as f64;
    
    let avg_recent_latency: f64 = history.iter()
        .skip(history.len() / 2)
        .map(|h| h.delivery_latency.average_ms)
        .sum::<f64>() / (history.len() - history.len() / 2) as f64;
    
    let latency_trend = if avg_recent_latency > avg_early_latency * 1.1 {
        "degrading"
    } else if avg_recent_latency < avg_early_latency * 0.9 {
        "improving"  
    } else {
        "stable"
    };
    
    // Calculate error rate trend
    let early_error_rate = if first.messages_published > 0 {
        (first.messages_failed as f64 / first.messages_published as f64) * 100.0
    } else {
        0.0
    };
    
    let recent_error_rate = if last.messages_published > 0 {
        (last.messages_failed as f64 / last.messages_published as f64) * 100.0
    } else {
        0.0
    };
    
    let error_rate_trend = recent_error_rate - early_error_rate;
    
    TrendsAnalysis {
        message_growth_rate,
        latency_trend: latency_trend.to_string(),
        peak_hours: vec![], // TODO: Implement peak hour detection
        error_rate_trend,
    }
}