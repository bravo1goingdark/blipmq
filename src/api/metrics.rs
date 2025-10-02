//! Comprehensive Metrics Collection System
//!
//! Provides detailed metrics and monitoring for BlipMQ:
//! - System performance metrics
//! - Message throughput and latency
//! - Resource utilization
//! - Error rates and patterns
//! - Custom business metrics

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::time::interval;
use tracing::{debug, info};

/// System-wide metrics collector
#[derive(Debug, Default)]
pub struct MetricsCollector {
    // Message metrics
    pub messages_published: AtomicU64,
    pub messages_delivered: AtomicU64,
    pub messages_acknowledged: AtomicU64,
    pub messages_failed: AtomicU64,
    pub messages_dropped_ttl: AtomicU64,
    pub messages_dropped_queue_full: AtomicU64,
    
    // Connection metrics
    pub connections_active: AtomicU64,
    pub connections_total: AtomicU64,
    pub connections_failed: AtomicU64,
    
    // Topic metrics
    pub topics_created: AtomicU64,
    pub topics_deleted: AtomicU64,
    pub subscribers_active: AtomicU64,
    
    // Performance metrics
    pub publish_latency_ms: LatencyMetrics,
    pub delivery_latency_ms: LatencyMetrics,
    
    // Resource metrics
    pub memory_usage_bytes: AtomicU64,
    pub disk_usage_bytes: AtomicU64,
    pub cpu_usage_percent: AtomicU64,
    
    // Per-topic metrics
    pub topic_metrics: DashMap<String, TopicMetrics>,
    
    // Historical data points
    pub historical_data: Arc<RwLock<Vec<MetricsSnapshot>>>,
    pub max_history_size: usize,
}

/// Latency tracking with percentiles
#[derive(Debug, Default)]
pub struct LatencyMetrics {
    pub count: AtomicU64,
    pub sum_ms: AtomicU64,
    pub min_ms: AtomicU64,
    pub max_ms: AtomicU64,
    // Simple histogram buckets (in production, use HdrHistogram)
    pub buckets: [AtomicU64; 10], // 0-1ms, 1-5ms, 5-10ms, etc.
}

impl LatencyMetrics {
    pub fn record(&self, latency_ms: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ms.fetch_add(latency_ms, Ordering::Relaxed);
        
        // Update min/max
        loop {
            let current_min = self.min_ms.load(Ordering::Relaxed);
            if latency_ms >= current_min && current_min != 0 {
                break;
            }
            if self.min_ms.compare_exchange_weak(
                current_min,
                latency_ms,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ).is_ok() {
                break;
            }
        }
        
        loop {
            let current_max = self.max_ms.load(Ordering::Relaxed);
            if latency_ms <= current_max {
                break;
            }
            if self.max_ms.compare_exchange_weak(
                current_max,
                latency_ms,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ).is_ok() {
                break;
            }
        }
        
        // Update histogram
        let bucket_index = match latency_ms {
            0..=1 => 0,
            2..=5 => 1,
            6..=10 => 2,
            11..=25 => 3,
            26..=50 => 4,
            51..=100 => 5,
            101..=250 => 6,
            251..=500 => 7,
            501..=1000 => 8,
            _ => 9,
        };
        self.buckets[bucket_index].fetch_add(1, Ordering::Relaxed);
    }
    
    pub fn average_ms(&self) -> f64 {
        let count = self.count.load(Ordering::Relaxed);
        if count == 0 {
            0.0
        } else {
            self.sum_ms.load(Ordering::Relaxed) as f64 / count as f64
        }
    }
    
    pub fn snapshot(&self) -> LatencySnapshot {
        LatencySnapshot {
            count: self.count.load(Ordering::Relaxed),
            average_ms: self.average_ms(),
            min_ms: self.min_ms.load(Ordering::Relaxed),
            max_ms: self.max_ms.load(Ordering::Relaxed),
            buckets: self.buckets.iter().map(|b| b.load(Ordering::Relaxed)).collect(),
        }
    }
}

/// Per-topic metrics
#[derive(Debug, Default)]
pub struct TopicMetrics {
    pub messages_published: AtomicU64,
    pub messages_delivered: AtomicU64,
    pub subscribers_count: AtomicU64,
    pub messages_in_queue: AtomicU64,
    pub bytes_published: AtomicU64,
    pub bytes_delivered: AtomicU64,
    pub last_message_at: AtomicU64,
    pub created_at: u64,
}

impl TopicMetrics {
    pub fn new() -> Self {
        Self {
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            ..Default::default()
        }
    }
    
    pub fn snapshot(&self, topic_name: &str) -> TopicMetricsSnapshot {
        TopicMetricsSnapshot {
            topic_name: topic_name.to_string(),
            messages_published: self.messages_published.load(Ordering::Relaxed),
            messages_delivered: self.messages_delivered.load(Ordering::Relaxed),
            subscribers_count: self.subscribers_count.load(Ordering::Relaxed),
            messages_in_queue: self.messages_in_queue.load(Ordering::Relaxed),
            bytes_published: self.bytes_published.load(Ordering::Relaxed),
            bytes_delivered: self.bytes_delivered.load(Ordering::Relaxed),
            last_message_at: self.last_message_at.load(Ordering::Relaxed),
            created_at: self.created_at,
        }
    }
}

/// Serializable metrics snapshots
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub timestamp: u64,
    pub messages_published: u64,
    pub messages_delivered: u64,
    pub messages_acknowledged: u64,
    pub messages_failed: u64,
    pub messages_dropped_ttl: u64,
    pub messages_dropped_queue_full: u64,
    pub connections_active: u64,
    pub connections_total: u64,
    pub connections_failed: u64,
    pub topics_created: u64,
    pub topics_deleted: u64,
    pub subscribers_active: u64,
    pub publish_latency: LatencySnapshot,
    pub delivery_latency: LatencySnapshot,
    pub memory_usage_bytes: u64,
    pub disk_usage_bytes: u64,
    pub cpu_usage_percent: u64,
    pub topic_metrics: Vec<TopicMetricsSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencySnapshot {
    pub count: u64,
    pub average_ms: f64,
    pub min_ms: u64,
    pub max_ms: u64,
    pub buckets: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicMetricsSnapshot {
    pub topic_name: String,
    pub messages_published: u64,
    pub messages_delivered: u64,
    pub subscribers_count: u64,
    pub messages_in_queue: u64,
    pub bytes_published: u64,
    pub bytes_delivered: u64,
    pub last_message_at: u64,
    pub created_at: u64,
}

impl MetricsCollector {
    pub fn new() -> Self {
        Self {
            max_history_size: 1000, // Keep 1000 data points
            ..Default::default()
        }
    }
    
    /// Records a message publication
    pub fn record_message_published(&self, topic: &str, size_bytes: usize) {
        self.messages_published.fetch_add(1, Ordering::Relaxed);
        
        let topic_metrics = self.topic_metrics
            .entry(topic.to_string())
            .or_insert_with(TopicMetrics::new);
        topic_metrics.messages_published.fetch_add(1, Ordering::Relaxed);
        topic_metrics.bytes_published.fetch_add(size_bytes as u64, Ordering::Relaxed);
        topic_metrics.last_message_at.store(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            Ordering::Relaxed,
        );
    }
    
    /// Records a message delivery
    pub fn record_message_delivered(&self, topic: &str, size_bytes: usize, latency_ms: u64) {
        self.messages_delivered.fetch_add(1, Ordering::Relaxed);
        self.delivery_latency_ms.record(latency_ms);
        
        if let Some(topic_metrics) = self.topic_metrics.get(topic) {
            topic_metrics.messages_delivered.fetch_add(1, Ordering::Relaxed);
            topic_metrics.bytes_delivered.fetch_add(size_bytes as u64, Ordering::Relaxed);
        }
    }
    
    /// Records a connection event
    pub fn record_connection_opened(&self) {
        self.connections_active.fetch_add(1, Ordering::Relaxed);
        self.connections_total.fetch_add(1, Ordering::Relaxed);
    }
    
    pub fn record_connection_closed(&self) {
        self.connections_active.fetch_sub(1, Ordering::Relaxed);
    }
    
    pub fn record_connection_failed(&self) {
        self.connections_failed.fetch_add(1, Ordering::Relaxed);
    }
    
    /// Records topic creation/deletion
    pub fn record_topic_created(&self, topic: &str) {
        self.topics_created.fetch_add(1, Ordering::Relaxed);
        self.topic_metrics.insert(topic.to_string(), TopicMetrics::new());
        info!("Topic created: {}", topic);
    }
    
    pub fn record_topic_deleted(&self, topic: &str) {
        self.topics_deleted.fetch_add(1, Ordering::Relaxed);
        self.topic_metrics.remove(topic);
        info!("Topic deleted: {}", topic);
    }
    
    /// Records subscriber events
    pub fn record_subscriber_added(&self, topic: &str) {
        self.subscribers_active.fetch_add(1, Ordering::Relaxed);
        
        if let Some(topic_metrics) = self.topic_metrics.get(topic) {
            topic_metrics.subscribers_count.fetch_add(1, Ordering::Relaxed);
        }
    }
    
    pub fn record_subscriber_removed(&self, topic: &str) {
        self.subscribers_active.fetch_sub(1, Ordering::Relaxed);
        
        if let Some(topic_metrics) = self.topic_metrics.get(topic) {
            topic_metrics.subscribers_count.fetch_sub(1, Ordering::Relaxed);
        }
    }
    
    /// Updates system resource metrics
    pub fn update_resource_metrics(&self) {
        // In production, integrate with system monitoring libraries
        use std::process::Command;
        
        // Get memory usage (rough estimation)
        if let Some(memory_usage) = get_process_memory_usage() {
            self.memory_usage_bytes.store(memory_usage, Ordering::Relaxed);
        }
    }
    
    /// Takes a snapshot of current metrics
    pub fn snapshot(&self) -> MetricsSnapshot {
        let topic_snapshots: Vec<TopicMetricsSnapshot> = self.topic_metrics
            .iter()
            .map(|entry| entry.value().snapshot(entry.key()))
            .collect();
        
        MetricsSnapshot {
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            messages_published: self.messages_published.load(Ordering::Relaxed),
            messages_delivered: self.messages_delivered.load(Ordering::Relaxed),
            messages_acknowledged: self.messages_acknowledged.load(Ordering::Relaxed),
            messages_failed: self.messages_failed.load(Ordering::Relaxed),
            messages_dropped_ttl: self.messages_dropped_ttl.load(Ordering::Relaxed),
            messages_dropped_queue_full: self.messages_dropped_queue_full.load(Ordering::Relaxed),
            connections_active: self.connections_active.load(Ordering::Relaxed),
            connections_total: self.connections_total.load(Ordering::Relaxed),
            connections_failed: self.connections_failed.load(Ordering::Relaxed),
            topics_created: self.topics_created.load(Ordering::Relaxed),
            topics_deleted: self.topics_deleted.load(Ordering::Relaxed),
            subscribers_active: self.subscribers_active.load(Ordering::Relaxed),
            publish_latency: self.publish_latency_ms.snapshot(),
            delivery_latency: self.delivery_latency_ms.snapshot(),
            memory_usage_bytes: self.memory_usage_bytes.load(Ordering::Relaxed),
            disk_usage_bytes: self.disk_usage_bytes.load(Ordering::Relaxed),
            cpu_usage_percent: self.cpu_usage_percent.load(Ordering::Relaxed),
            topic_metrics: topic_snapshots,
        }
    }
    
    /// Stores a metrics snapshot in history
    pub fn store_snapshot(&self) {
        let snapshot = self.snapshot();
        let mut history = self.historical_data.write();
        
        history.push(snapshot);
        
        // Keep only the most recent entries
        if history.len() > self.max_history_size {
            history.remove(0);
        }
    }
    
    /// Gets historical metrics data
    pub fn get_history(&self, limit: Option<usize>) -> Vec<MetricsSnapshot> {
        let history = self.historical_data.read();
        let limit = limit.unwrap_or(history.len());
        
        if limit >= history.len() {
            history.clone()
        } else {
            history[history.len() - limit..].to_vec()
        }
    }
    
    /// Starts background metrics collection
    pub fn start_background_collection(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(60)); // Collect every minute
            
            loop {
                interval.tick().await;
                
                // Update resource metrics
                self.update_resource_metrics();
                
                // Store snapshot
                self.store_snapshot();
                
                debug!("Metrics snapshot stored");
            }
        });
    }
}

/// Gets process memory usage (platform-specific)
fn get_process_memory_usage() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        use std::fs;
        if let Ok(contents) = fs::read_to_string("/proc/self/statm") {
            if let Some(first) = contents.split_whitespace().next() {
                if let Ok(pages) = first.parse::<u64>() {
                    return Some(pages * 4096); // Assume 4KB page size
                }
            }
        }
    }
    
    #[cfg(target_os = "windows")]
    {
        // Windows implementation would use WinAPI
        // For simplicity, return a placeholder
        return Some(1024 * 1024 * 100); // 100MB placeholder
    }
    
    #[cfg(target_os = "macos")]
    {
        // macOS implementation would use task_info
        return Some(1024 * 1024 * 100); // 100MB placeholder
    }
    
    None
}

/// Global metrics instance
static METRICS: once_cell::sync::Lazy<Arc<MetricsCollector>> = 
    once_cell::sync::Lazy::new(|| {
        let metrics = Arc::new(MetricsCollector::new());
        let metrics_clone = Arc::clone(&metrics);
        metrics_clone.start_background_collection();
        metrics
    });

/// Convenience functions for recording metrics
pub fn inc_published(topic: &str, size_bytes: usize) {
    METRICS.record_message_published(topic, size_bytes);
}

pub fn inc_delivered(topic: &str, size_bytes: usize, latency_ms: u64) {
    METRICS.record_message_delivered(topic, size_bytes, latency_ms);
}

pub fn inc_acknowledged() {
    METRICS.messages_acknowledged.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_failed() {
    METRICS.messages_failed.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_dropped_ttl(count: u64) {
    METRICS.messages_dropped_ttl.fetch_add(count, Ordering::Relaxed);
}

pub fn inc_dropped_queue_full(count: u64) {
    METRICS.messages_dropped_queue_full.fetch_add(count, Ordering::Relaxed);
}

pub fn inc_enqueued(count: u64) {
    // This is already tracked in delivered metrics, but we can add separate tracking if needed
}

pub fn connection_opened() {
    METRICS.record_connection_opened();
}

pub fn connection_closed() {
    METRICS.record_connection_closed();
}

pub fn connection_failed() {
    METRICS.record_connection_failed();
}

pub fn topic_created(topic: &str) {
    METRICS.record_topic_created(topic);
}

pub fn topic_deleted(topic: &str) {
    METRICS.record_topic_deleted(topic);
}

pub fn subscriber_added(topic: &str) {
    METRICS.record_subscriber_added(topic);
}

pub fn subscriber_removed(topic: &str) {
    METRICS.record_subscriber_removed(topic);
}

/// Gets the global metrics instance
pub fn global_metrics() -> Arc<MetricsCollector> {
    Arc::clone(&METRICS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::sleep;

    #[test]
    fn test_latency_metrics() {
        let latency = LatencyMetrics::default();
        
        latency.record(5);
        latency.record(15);
        latency.record(25);
        
        let snapshot = latency.snapshot();
        assert_eq!(snapshot.count, 3);
        assert_eq!(snapshot.average_ms, 15.0);
        assert_eq!(snapshot.min_ms, 5);
        assert_eq!(snapshot.max_ms, 25);
    }
    
    #[test]
    fn test_metrics_collector() {
        let collector = MetricsCollector::new();
        
        collector.record_message_published("test-topic", 100);
        collector.record_message_delivered("test-topic", 100, 5);
        collector.record_connection_opened();
        
        let snapshot = collector.snapshot();
        assert_eq!(snapshot.messages_published, 1);
        assert_eq!(snapshot.messages_delivered, 1);
        assert_eq!(snapshot.connections_active, 1);
        assert_eq!(snapshot.connections_total, 1);
        assert_eq!(snapshot.topic_metrics.len(), 1);
    }
    
    #[tokio::test]
    async fn test_global_metrics() {
        inc_published("test-topic", 100);
        inc_delivered("test-topic", 100, 10);
        connection_opened();
        
        let metrics = global_metrics();
        let snapshot = metrics.snapshot();
        
        assert!(snapshot.messages_published >= 1);
        assert!(snapshot.messages_delivered >= 1);
        assert!(snapshot.connections_active >= 1);
    }
}