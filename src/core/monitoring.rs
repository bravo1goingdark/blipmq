//! Advanced monitoring, metrics collection, and health checks.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::net::TcpListener;
use tokio::io::{AsyncWriteExt, BufWriter};
use serde_json::json;
use crate::core::performance::PERFORMANCE_METRICS;
use crate::core::memory_pool::POOL_STATS;

/// Health check status
#[derive(Debug, Clone, PartialEq)]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

impl std::fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HealthStatus::Healthy => write!(f, "healthy"),
            HealthStatus::Degraded => write!(f, "degraded"), 
            HealthStatus::Unhealthy => write!(f, "unhealthy"),
        }
    }
}

/// Health check result
#[derive(Debug, Clone)]
pub struct HealthCheck {
    pub component: String,
    pub status: HealthStatus,
    pub message: String,
    pub response_time_ms: u64,
    pub last_check: SystemTime,
}

/// System metrics collector
#[derive(Debug)]
pub struct MetricsCollector {
    start_time: Instant,
    metrics_history: parking_lot::RwLock<Vec<MetricsSnapshot>>,
    health_checks: parking_lot::RwLock<HashMap<String, HealthCheck>>,
}

#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub timestamp: SystemTime,
    pub connections_active: u64,
    pub connections_total: u64,
    pub messages_published: u64,
    pub messages_delivered: u64,
    pub messages_dropped: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub memory_used: u64,
    pub cpu_usage_percent: f64,
    pub throughput_msg_per_sec: f64,
    pub avg_latency_ms: f64,
    pub p99_latency_ms: f64,
    pub buffer_pool_hit_rate: f64,
    pub message_pool_hit_rate: f64,
}

impl MetricsCollector {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            metrics_history: parking_lot::RwLock::new(Vec::new()),
            health_checks: parking_lot::RwLock::new(HashMap::new()),
        }
    }
    
    /// Collect current metrics snapshot
    pub fn collect_snapshot(&self) -> MetricsSnapshot {
        let metrics = &*PERFORMANCE_METRICS;
        let pool_stats = &*POOL_STATS;
        
        MetricsSnapshot {
            timestamp: SystemTime::now(),
            connections_active: metrics.active_connections.load(Ordering::Relaxed),
            connections_total: metrics.connections_accepted.load(Ordering::Relaxed),
            messages_published: metrics.messages_published.load(Ordering::Relaxed),
            messages_delivered: metrics.messages_delivered.load(Ordering::Relaxed),
            messages_dropped: metrics.messages_dropped.load(Ordering::Relaxed),
            bytes_sent: metrics.bytes_sent.load(Ordering::Relaxed),
            bytes_received: metrics.bytes_received.load(Ordering::Relaxed),
            memory_used: metrics.memory_usage.load(Ordering::Relaxed),
            cpu_usage_percent: metrics.cpu_usage.load(Ordering::Relaxed) as f64 / 100.0,
            throughput_msg_per_sec: metrics.throughput_per_second(Duration::from_secs(1)),
            avg_latency_ms: self.calculate_avg_latency(),
            p99_latency_ms: metrics.get_publish_latency_p99()
                .unwrap_or_default()
                .as_micros() as f64 / 1000.0,
            buffer_pool_hit_rate: pool_stats.buffer_hit_rate(),
            message_pool_hit_rate: pool_stats.message_hit_rate(),
        }
    }
    
    /// Store metrics snapshot in history
    pub fn store_snapshot(&self, snapshot: MetricsSnapshot) {
        let mut history = self.metrics_history.write();
        history.push(snapshot);
        
        // Keep only last 1000 snapshots (about 16 minutes at 1s intervals)
        if history.len() > 1000 {
            history.remove(0);
        }
    }
    
    /// Get metrics history
    pub fn get_history(&self) -> Vec<MetricsSnapshot> {
        self.metrics_history.read().clone()
    }
    
    /// Run health checks
    pub async fn run_health_checks(&self) {
        let mut checks = HashMap::new();
        
        // Memory health check
        let memory_check = self.check_memory_health().await;
        checks.insert("memory".to_string(), memory_check);
        
        // Connection health check
        let connection_check = self.check_connection_health().await;
        checks.insert("connections".to_string(), connection_check);
        
        // Latency health check
        let latency_check = self.check_latency_health().await;
        checks.insert("latency".to_string(), latency_check);
        
        // Throughput health check
        let throughput_check = self.check_throughput_health().await;
        checks.insert("throughput".to_string(), throughput_check);
        
        // Update stored health checks
        *self.health_checks.write() = checks;
    }
    
    async fn check_memory_health(&self) -> HealthCheck {
        let start = Instant::now();
        let memory_used = PERFORMANCE_METRICS.memory_usage.load(Ordering::Relaxed);
        let memory_limit = 4 * 1024 * 1024 * 1024u64; // 4GB limit
        
        let usage_percent = (memory_used as f64 / memory_limit as f64) * 100.0;
        
        let (status, message) = if usage_percent > 90.0 {
            (HealthStatus::Unhealthy, format!("Memory usage critical: {:.1}%", usage_percent))
        } else if usage_percent > 75.0 {
            (HealthStatus::Degraded, format!("Memory usage high: {:.1}%", usage_percent))
        } else {
            (HealthStatus::Healthy, format!("Memory usage normal: {:.1}%", usage_percent))
        };
        
        HealthCheck {
            component: "memory".to_string(),
            status,
            message,
            response_time_ms: start.elapsed().as_millis() as u64,
            last_check: SystemTime::now(),
        }
    }
    
    async fn check_connection_health(&self) -> HealthCheck {
        let start = Instant::now();
        let active_connections = PERFORMANCE_METRICS.active_connections.load(Ordering::Relaxed);
        let max_connections = 10000u64; // From config
        
        let usage_percent = (active_connections as f64 / max_connections as f64) * 100.0;
        
        let (status, message) = if usage_percent > 95.0 {
            (HealthStatus::Unhealthy, format!("Connection limit critical: {}/{}", active_connections, max_connections))
        } else if usage_percent > 80.0 {
            (HealthStatus::Degraded, format!("Connection usage high: {}/{}", active_connections, max_connections))
        } else {
            (HealthStatus::Healthy, format!("Connections normal: {}/{}", active_connections, max_connections))
        };
        
        HealthCheck {
            component: "connections".to_string(),
            status,
            message,
            response_time_ms: start.elapsed().as_millis() as u64,
            last_check: SystemTime::now(),
        }
    }
    
    async fn check_latency_health(&self) -> HealthCheck {
        let start = Instant::now();
        
        let p99_latency_ms = PERFORMANCE_METRICS
            .get_publish_latency_p99()
            .unwrap_or_default()
            .as_millis() as u64;
        
        let (status, message) = if p99_latency_ms > 100 {
            (HealthStatus::Unhealthy, format!("P99 latency critical: {}ms", p99_latency_ms))
        } else if p99_latency_ms > 50 {
            (HealthStatus::Degraded, format!("P99 latency elevated: {}ms", p99_latency_ms))
        } else {
            (HealthStatus::Healthy, format!("P99 latency good: {}ms", p99_latency_ms))
        };
        
        HealthCheck {
            component: "latency".to_string(),
            status,
            message,
            response_time_ms: start.elapsed().as_millis() as u64,
            last_check: SystemTime::now(),
        }
    }
    
    async fn check_throughput_health(&self) -> HealthCheck {
        let start = Instant::now();
        let throughput = PERFORMANCE_METRICS.throughput_per_second(Duration::from_secs(10));
        
        let (status, message) = if throughput < 100.0 {
            (HealthStatus::Degraded, format!("Low throughput: {:.1} msg/s", throughput))
        } else {
            (HealthStatus::Healthy, format!("Throughput normal: {:.1} msg/s", throughput))
        };
        
        HealthCheck {
            component: "throughput".to_string(),
            status,
            message,
            response_time_ms: start.elapsed().as_millis() as u64,
            last_check: SystemTime::now(),
        }
    }
    
    /// Get overall system health
    pub fn get_overall_health(&self) -> HealthStatus {
        let checks = self.health_checks.read();
        
        let mut healthy_count = 0;
        let mut degraded_count = 0;
        let mut unhealthy_count = 0;
        
        for check in checks.values() {
            match check.status {
                HealthStatus::Healthy => healthy_count += 1,
                HealthStatus::Degraded => degraded_count += 1,
                HealthStatus::Unhealthy => unhealthy_count += 1,
            }
        }
        
        if unhealthy_count > 0 {
            HealthStatus::Unhealthy
        } else if degraded_count > 0 {
            HealthStatus::Degraded
        } else {
            HealthStatus::Healthy
        }
    }
    
    /// Get health check results
    pub fn get_health_checks(&self) -> HashMap<String, HealthCheck> {
        self.health_checks.read().clone()
    }
    
    /// Generate Prometheus metrics
    pub fn to_prometheus(&self) -> String {
        let snapshot = self.collect_snapshot();
        let uptime = self.start_time.elapsed().as_secs();
        
        format!(r#"# HELP blipmq_uptime_seconds Total uptime in seconds
# TYPE blipmq_uptime_seconds counter
blipmq_uptime_seconds {}

# HELP blipmq_connections_active Currently active connections
# TYPE blipmq_connections_active gauge
blipmq_connections_active {}

# HELP blipmq_connections_total Total connections accepted
# TYPE blipmq_connections_total counter
blipmq_connections_total {}

# HELP blipmq_messages_published_total Total messages published
# TYPE blipmq_messages_published_total counter
blipmq_messages_published_total {}

# HELP blipmq_messages_delivered_total Total messages delivered
# TYPE blipmq_messages_delivered_total counter
blipmq_messages_delivered_total {}

# HELP blipmq_messages_dropped_total Total messages dropped
# TYPE blipmq_messages_dropped_total counter
blipmq_messages_dropped_total {}

# HELP blipmq_bytes_sent_total Total bytes sent
# TYPE blipmq_bytes_sent_total counter
blipmq_bytes_sent_total {}

# HELP blipmq_bytes_received_total Total bytes received
# TYPE blipmq_bytes_received_total counter
blipmq_bytes_received_total {}

# HELP blipmq_memory_used_bytes Currently used memory in bytes
# TYPE blipmq_memory_used_bytes gauge
blipmq_memory_used_bytes {}

# HELP blipmq_cpu_usage_percent CPU usage percentage
# TYPE blipmq_cpu_usage_percent gauge
blipmq_cpu_usage_percent {}

# HELP blipmq_throughput_messages_per_second Current throughput in messages per second
# TYPE blipmq_throughput_messages_per_second gauge
blipmq_throughput_messages_per_second {}

# HELP blipmq_latency_p99_milliseconds 99th percentile latency in milliseconds
# TYPE blipmq_latency_p99_milliseconds gauge
blipmq_latency_p99_milliseconds {}

# HELP blipmq_buffer_pool_hit_rate Buffer pool hit rate
# TYPE blipmq_buffer_pool_hit_rate gauge
blipmq_buffer_pool_hit_rate {}

# HELP blipmq_message_pool_hit_rate Message pool hit rate
# TYPE blipmq_message_pool_hit_rate gauge
blipmq_message_pool_hit_rate {}
"#,
            uptime,
            snapshot.connections_active,
            snapshot.connections_total,
            snapshot.messages_published,
            snapshot.messages_delivered,
            snapshot.messages_dropped,
            snapshot.bytes_sent,
            snapshot.bytes_received,
            snapshot.memory_used,
            snapshot.cpu_usage_percent,
            snapshot.throughput_msg_per_sec,
            snapshot.p99_latency_ms,
            snapshot.buffer_pool_hit_rate,
            snapshot.message_pool_hit_rate
        )
    }
    
    /// Generate JSON metrics
    pub fn to_json(&self) -> serde_json::Value {
        let snapshot = self.collect_snapshot();
        let uptime = self.start_time.elapsed().as_secs();
        let health_checks = self.get_health_checks();
        let overall_health = self.get_overall_health();
        
        json!({
            "timestamp": snapshot.timestamp.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
            "uptime_seconds": uptime,
            "health": {
                "overall": overall_health.to_string(),
                "checks": health_checks
            },
            "connections": {
                "active": snapshot.connections_active,
                "total": snapshot.connections_total
            },
            "messages": {
                "published": snapshot.messages_published,
                "delivered": snapshot.messages_delivered,
                "dropped": snapshot.messages_dropped
            },
            "traffic": {
                "bytes_sent": snapshot.bytes_sent,
                "bytes_received": snapshot.bytes_received,
                "throughput_msg_per_sec": snapshot.throughput_msg_per_sec
            },
            "performance": {
                "avg_latency_ms": snapshot.avg_latency_ms,
                "p99_latency_ms": snapshot.p99_latency_ms,
                "cpu_usage_percent": snapshot.cpu_usage_percent,
                "memory_used_bytes": snapshot.memory_used
            },
            "memory_pools": {
                "buffer_hit_rate": snapshot.buffer_pool_hit_rate,
                "message_hit_rate": snapshot.message_pool_hit_rate
            }
        })
    }
    
    fn calculate_avg_latency(&self) -> f64 {
        // Simplified average latency calculation
        // In real implementation, this would use histogram data
        PERFORMANCE_METRICS
            .get_publish_latency_p99()
            .unwrap_or_default()
            .as_micros() as f64 / 2000.0 // Rough estimate: avg ≈ p99/2
    }
}

/// HTTP metrics server
pub struct MetricsServer {
    collector: Arc<MetricsCollector>,
    bind_addr: String,
}

impl MetricsServer {
    pub fn new(bind_addr: String) -> Self {
        Self {
            collector: Arc::new(MetricsCollector::new()),
            bind_addr,
        }
    }
    
    /// Start metrics collection and HTTP server
    pub async fn start(&self) -> anyhow::Result<()> {
        // Start background metrics collection
        let collector = Arc::clone(&self.collector);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;
                let snapshot = collector.collect_snapshot();
                collector.store_snapshot(snapshot);
                collector.run_health_checks().await;
            }
        });
        
        // Start HTTP server
        let listener = TcpListener::bind(&self.bind_addr).await?;
        tracing::info!("📊 Metrics server started on {}", self.bind_addr);
        
        let collector = Arc::clone(&self.collector);
        loop {
            let (stream, _) = listener.accept().await?;
            let collector = Arc::clone(&collector);
            
            tokio::spawn(async move {
                if let Err(e) = Self::handle_request(stream, collector).await {
                    tracing::error!("Metrics request error: {}", e);
                }
            });
        }
    }
    
    async fn handle_request(
        mut stream: tokio::net::TcpStream,
        collector: Arc<MetricsCollector>,
    ) -> anyhow::Result<()> {
        use tokio::io::AsyncReadExt;
        
        // Read request (simplified HTTP parsing)
        let mut buffer = [0; 1024];
        let n = stream.read(&mut buffer).await?;
        let request = String::from_utf8_lossy(&buffer[..n]);
        
        let (status, content_type, body) = if request.contains("GET /metrics") {
            ("200 OK", "text/plain; version=0.0.4", collector.to_prometheus())
        } else if request.contains("GET /health") {
            let health = collector.get_overall_health();
            let status_code = match health {
                HealthStatus::Healthy => "200 OK",
                HealthStatus::Degraded => "200 OK", 
                HealthStatus::Unhealthy => "503 Service Unavailable",
            };
            (status_code, "application/json", collector.to_json().to_string())
        } else if request.contains("GET /status") {
            ("200 OK", "application/json", collector.to_json().to_string())
        } else {
            ("404 Not Found", "text/plain", "Not Found".to_string())
        };
        
        // Send HTTP response
        let response = format!(
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n{}",
            status,
            content_type,
            body.len(),
            body
        );
        
        stream.write_all(response.as_bytes()).await?;
        stream.flush().await?;
        
        Ok(())
    }
}

/// Start monitoring services
pub async fn start_monitoring(metrics_addr: String) -> anyhow::Result<()> {
    let metrics_server = MetricsServer::new(metrics_addr);
    metrics_server.start().await
}