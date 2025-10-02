//! Dead Letter Queue (DLQ) Implementation
//!
//! Handles failed messages that cannot be delivered or processed:
//! - Automatic routing to DLQ after max retries
//! - Configurable DLQ policies per topic
//! - Message analysis and failure categorization
//! - DLQ monitoring and alerting
//! - Manual message recovery and replay

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::interval;
use tracing::{debug, error, info, warn};

use crate::core::message::WireMessage;
use crate::core::qos::{Acknowledgment, AckType};
use crate::core::wal::{WalEntryType, WriteAheadLog};

/// Dead Letter Queue Policy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DlqPolicy {
    pub name: String,
    pub max_delivery_attempts: u32,
    pub max_dlq_size: usize,
    pub retention_duration_ms: u64,
    pub enable_analytics: bool,
    pub auto_purge_expired: bool,
    pub alert_thresholds: AlertThresholds,
}

impl Default for DlqPolicy {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            max_delivery_attempts: 3,
            max_dlq_size: 10_000,
            retention_duration_ms: 7 * 24 * 60 * 60 * 1000, // 7 days
            enable_analytics: true,
            auto_purge_expired: true,
            alert_thresholds: AlertThresholds::default(),
        }
    }
}

/// Alert thresholds for DLQ monitoring
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertThresholds {
    pub size_threshold: usize,
    pub size_threshold_percentage: f32,
    pub error_rate_threshold: f32,
    pub alert_interval_ms: u64,
}

impl Default for AlertThresholds {
    fn default() -> Self {
        Self {
            size_threshold: 1000,
            size_threshold_percentage: 0.8, // 80% of max size
            error_rate_threshold: 0.1,       // 10% error rate
            alert_interval_ms: 5 * 60 * 1000, // 5 minutes
        }
    }
}

/// Failure categories for message analysis
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FailureCategory {
    /// Temporary network or connection issues
    NetworkError,
    /// Message format or validation errors
    FormatError,
    /// Subscriber processing errors
    ProcessingError,
    /// Authentication or authorization failures
    AuthError,
    /// Resource exhaustion (memory, disk, etc.)
    ResourceError,
    /// Timeout during processing
    TimeoutError,
    /// Unknown or unclassified error
    Unknown,
}

impl From<&str> for FailureCategory {
    fn from(reason: &str) -> Self {
        let reason_lower = reason.to_lowercase();
        if reason_lower.contains("network") || reason_lower.contains("connection") {
            FailureCategory::NetworkError
        } else if reason_lower.contains("format") || reason_lower.contains("validation") {
            FailureCategory::FormatError
        } else if reason_lower.contains("processing") || reason_lower.contains("handler") {
            FailureCategory::ProcessingError
        } else if reason_lower.contains("auth") || reason_lower.contains("permission") {
            FailureCategory::AuthError
        } else if reason_lower.contains("memory") || reason_lower.contains("disk") {
            FailureCategory::ResourceError
        } else if reason_lower.contains("timeout") {
            FailureCategory::TimeoutError
        } else {
            FailureCategory::Unknown
        }
    }
}

/// Dead letter message with failure information
#[derive(Debug, Clone)]
pub struct DeadLetterMessage {
    pub original_message: Arc<WireMessage>,
    pub topic: String,
    pub subscriber_id: String,
    pub failure_reason: String,
    pub failure_category: FailureCategory,
    pub delivery_attempts: u32,
    pub first_failure_at: u64,
    pub last_failure_at: u64,
    pub dlq_entry_time: u64,
    pub can_retry: bool,
    pub original_acks: Vec<Acknowledgment>,
}

impl DeadLetterMessage {
    pub fn new(
        message: Arc<WireMessage>,
        topic: String,
        subscriber_id: String,
        failure_reason: String,
        delivery_attempts: u32,
        original_acks: Vec<Acknowledgment>,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let failure_category = FailureCategory::from(failure_reason.as_str());
        let can_retry = matches!(
            failure_category,
            FailureCategory::NetworkError | FailureCategory::TimeoutError | FailureCategory::ResourceError
        );

        Self {
            original_message: message,
            topic,
            subscriber_id,
            failure_reason,
            failure_category,
            delivery_attempts,
            first_failure_at: now,
            last_failure_at: now,
            dlq_entry_time: now,
            can_retry,
            original_acks,
        }
    }

    pub fn is_expired(&self, retention_duration_ms: u64) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        
        now - self.dlq_entry_time > retention_duration_ms
    }
}

/// DLQ statistics for monitoring
#[derive(Debug, Default)]
pub struct DlqStats {
    pub total_messages: AtomicU64,
    pub messages_by_category: DashMap<FailureCategory, AtomicU64>,
    pub messages_by_topic: DashMap<String, AtomicU64>,
    pub messages_recovered: AtomicU64,
    pub messages_purged: AtomicU64,
    pub alerts_sent: AtomicU64,
    pub current_size: AtomicU64,
    pub max_size_reached: AtomicU64,
}

/// Serializable snapshot of DLQ statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DlqStatsSnapshot {
    pub total_messages: u64,
    pub messages_recovered: u64,
    pub messages_purged: u64,
    pub alerts_sent: u64,
    pub current_size: u64,
    pub max_size_reached: u64,
}

impl DlqStats {
    pub fn record_message(&self, dlq_message: &DeadLetterMessage) {
        self.total_messages.fetch_add(1, Ordering::Relaxed);
        self.current_size.fetch_add(1, Ordering::Relaxed);
        
        // Update category stats
        let category_count = self.messages_by_category
            .entry(dlq_message.failure_category.clone())
            .or_insert_with(|| AtomicU64::new(0));
        category_count.fetch_add(1, Ordering::Relaxed);
        
        // Update topic stats
        let topic_count = self.messages_by_topic
            .entry(dlq_message.topic.clone())
            .or_insert_with(|| AtomicU64::new(0));
        topic_count.fetch_add(1, Ordering::Relaxed);
        
        // Update max size if needed
        let current_size = self.current_size.load(Ordering::Relaxed);
        let max_size = self.max_size_reached.load(Ordering::Relaxed);
        if current_size > max_size {
            self.max_size_reached.store(current_size, Ordering::Relaxed);
        }
    }

    pub fn record_recovery(&self) {
        self.messages_recovered.fetch_add(1, Ordering::Relaxed);
        self.current_size.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn record_purge(&self, count: u64) {
        self.messages_purged.fetch_add(count, Ordering::Relaxed);
        self.current_size.fetch_sub(count, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> DlqStatsSnapshot {
        let mut category_stats = HashMap::new();
        for entry in self.messages_by_category.iter() {
            category_stats.insert(
                entry.key().clone(),
                entry.value().load(Ordering::Relaxed),
            );
        }

        let mut topic_stats = HashMap::new();
        for entry in self.messages_by_topic.iter() {
            topic_stats.insert(
                entry.key().clone(),
                entry.value().load(Ordering::Relaxed),
            );
        }

        DlqStatsSnapshot {
            total_messages: self.total_messages.load(Ordering::Relaxed),
            messages_recovered: self.messages_recovered.load(Ordering::Relaxed),
            messages_purged: self.messages_purged.load(Ordering::Relaxed),
            alerts_sent: self.alerts_sent.load(Ordering::Relaxed),
            current_size: self.current_size.load(Ordering::Relaxed),
            max_size_reached: self.max_size_reached.load(Ordering::Relaxed),
        }
    }
}


/// Alert information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DlqAlert {
    pub alert_type: DlqAlertType,
    pub topic: String,
    pub message: String,
    pub current_value: f64,
    pub threshold: f64,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DlqAlertType {
    SizeThreshold,
    ErrorRate,
    MaxSizeReached,
}

/// Dead Letter Queue Manager
pub struct DeadLetterQueue {
    /// DLQ messages by topic
    messages_by_topic: DashMap<String, Vec<DeadLetterMessage>>,
    
    /// DLQ policies by topic
    policies: Arc<RwLock<HashMap<String, DlqPolicy>>>,
    
    /// Default policy
    default_policy: DlqPolicy,
    
    /// Statistics
    stats: DlqStats,
    
    /// Write-ahead log for durability
    wal: Option<Arc<WriteAheadLog>>,
    
    /// Alert channel
    alert_tx: mpsc::UnboundedSender<DlqAlert>,
    
    /// Last alert times by topic
    last_alert_times: DashMap<String, u64>,
}

impl DeadLetterQueue {
    pub fn new(wal: Option<Arc<WriteAheadLog>>) -> Self {
        let (alert_tx, alert_rx) = mpsc::unbounded_channel();
        
        let dlq = Self {
            messages_by_topic: DashMap::new(),
            policies: Arc::new(RwLock::new(HashMap::new())),
            default_policy: DlqPolicy::default(),
            stats: DlqStats::default(),
            wal,
            alert_tx,
            last_alert_times: DashMap::new(),
        };

        // Start background tasks
        dlq.start_alert_handler(alert_rx);
        dlq.start_maintenance_task();

        dlq
    }

    /// Adds a message to the DLQ
    pub async fn add_message(&self, dlq_message: DeadLetterMessage) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let topic = dlq_message.topic.clone();
        let policy = self.get_policy_for_topic(&topic);
        
        // Check if DLQ is at capacity
        let current_topic_size = self.messages_by_topic
            .get(&topic)
            .map(|messages| messages.len())
            .unwrap_or(0);
            
        if current_topic_size >= policy.max_dlq_size {
            if policy.auto_purge_expired {
                // Try to purge expired messages first
                let purged = self.purge_expired_messages(&topic).await?;
                info!("Purged {} expired messages from DLQ for topic {}", purged, topic);
            }
            
            // Check again after purge
            let current_size = self.messages_by_topic
                .get(&topic)
                .map(|messages| messages.len())
                .unwrap_or(0);
                
            if current_size >= policy.max_dlq_size {
                // Remove oldest message to make space
                if let Some(mut messages) = self.messages_by_topic.get_mut(&topic) {
                    if !messages.is_empty() {
                        messages.remove(0);
                        warn!("Removed oldest DLQ message for topic {} due to capacity limit", topic);
                    }
                }
            }
        }

        // Add the message
        self.messages_by_topic
            .entry(topic.clone())
            .or_insert_with(Vec::new)
            .push(dlq_message.clone());

        // Record statistics
        self.stats.record_message(&dlq_message);

        // Persist to WAL if available
        if let Some(ref wal) = self.wal {
            wal.write_entry(WalEntryType::MessagePublished {
                topic: format!("__dlq_{}", topic),
                message_id: extract_message_id(&dlq_message.original_message)?,
                payload: dlq_message.original_message.frame.clone(),
                timestamp: dlq_message.dlq_entry_time,
                ttl_ms: policy.retention_duration_ms,
            }).await?;
        }

        // Check for alerts
        self.check_and_send_alerts(&topic, &policy).await;

        info!("Added message to DLQ for topic {}: {}", topic, dlq_message.failure_reason);
        Ok(())
    }

    /// Recovers a message from DLQ for retry
    pub async fn recover_message(
        &self,
        topic: &str,
        message_index: usize,
    ) -> Result<Option<DeadLetterMessage>, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(mut messages) = self.messages_by_topic.get_mut(topic) {
            if message_index < messages.len() {
                let recovered_message = messages.remove(message_index);
                self.stats.record_recovery();
                
                info!("Recovered message from DLQ for topic {}", topic);
                return Ok(Some(recovered_message));
            }
        }
        
        Ok(None)
    }

    /// Lists messages in DLQ for a topic
    pub fn list_messages(&self, topic: &str, limit: Option<usize>) -> Vec<DeadLetterMessage> {
        if let Some(messages) = self.messages_by_topic.get(topic) {
            let limit = limit.unwrap_or(messages.len());
            messages.iter().take(limit).cloned().collect()
        } else {
            Vec::new()
        }
    }

    /// Purges expired messages from DLQ
    pub async fn purge_expired_messages(&self, topic: &str) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        let policy = self.get_policy_for_topic(topic);
        let mut purged_count = 0u64;

        if let Some(mut messages) = self.messages_by_topic.get_mut(topic) {
            let original_len = messages.len();
            messages.retain(|msg| !msg.is_expired(policy.retention_duration_ms));
            purged_count = (original_len - messages.len()) as u64;
            
            if purged_count > 0 {
                self.stats.record_purge(purged_count);
                info!("Purged {} expired messages from DLQ for topic {}", purged_count, topic);
            }
        }

        Ok(purged_count)
    }

    /// Purges all messages from DLQ for a topic
    pub async fn purge_all_messages(&self, topic: &str) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(mut messages) = self.messages_by_topic.get_mut(topic) {
            let purged_count = messages.len() as u64;
            messages.clear();
            self.stats.record_purge(purged_count);
            
            info!("Purged all {} messages from DLQ for topic {}", purged_count, topic);
            return Ok(purged_count);
        }
        
        Ok(0)
    }

    /// Sets DLQ policy for a topic
    pub fn set_policy(&self, topic: String, policy: DlqPolicy) {
        self.policies.write().insert(topic, policy);
    }

    /// Gets DLQ policy for a topic
    pub fn get_policy_for_topic(&self, topic: &str) -> DlqPolicy {
        self.policies
            .read()
            .get(topic)
            .cloned()
            .unwrap_or_else(|| self.default_policy.clone())
    }

    /// Gets statistics
    pub fn stats(&self) -> DlqStatsSnapshot {
        self.stats.snapshot()
    }

    /// Checks and sends alerts if thresholds are exceeded
    async fn check_and_send_alerts(&self, topic: &str, policy: &DlqPolicy) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        // Check if enough time has passed since last alert
        if let Some(last_alert) = self.last_alert_times.get(topic) {
            if now - *last_alert < policy.alert_thresholds.alert_interval_ms {
                return; // Too soon to send another alert
            }
        }

        let current_size = self.messages_by_topic
            .get(topic)
            .map(|messages| messages.len())
            .unwrap_or(0);

        // Check size threshold
        if current_size >= policy.alert_thresholds.size_threshold {
            let alert = DlqAlert {
                alert_type: DlqAlertType::SizeThreshold,
                topic: topic.to_string(),
                message: format!("DLQ size ({}) exceeded threshold ({})", current_size, policy.alert_thresholds.size_threshold),
                current_value: current_size as f64,
                threshold: policy.alert_thresholds.size_threshold as f64,
                timestamp: now,
            };

            if let Err(_) = self.alert_tx.send(alert) {
                error!("Failed to send DLQ alert for topic {}", topic);
            } else {
                self.last_alert_times.insert(topic.to_string(), now);
                self.stats.alerts_sent.fetch_add(1, Ordering::Relaxed);
            }
        }

        // Check percentage threshold
        let size_percentage = current_size as f32 / policy.max_dlq_size as f32;
        if size_percentage >= policy.alert_thresholds.size_threshold_percentage {
            let alert = DlqAlert {
                alert_type: DlqAlertType::MaxSizeReached,
                topic: topic.to_string(),
                message: format!("DLQ is {:.1}% full ({}/{})", 
                    size_percentage * 100.0, current_size, policy.max_dlq_size),
                current_value: size_percentage as f64,
                threshold: policy.alert_thresholds.size_threshold_percentage as f64,
                timestamp: now,
            };

            if let Err(_) = self.alert_tx.send(alert) {
                error!("Failed to send DLQ percentage alert for topic {}", topic);
            }
        }
    }

    /// Starts the alert handler
    fn start_alert_handler(&self, mut alert_rx: mpsc::UnboundedReceiver<DlqAlert>) {
        tokio::spawn(async move {
            while let Some(alert) = alert_rx.recv().await {
                // In a real implementation, this would integrate with alerting systems
                // (email, Slack, PagerDuty, etc.)
                warn!("DLQ Alert: {} - {}", alert.topic, alert.message);
                
                // Log structured alert data
                info!(
                    alert_type = ?alert.alert_type,
                    topic = alert.topic,
                    message = alert.message,
                    current_value = alert.current_value,
                    threshold = alert.threshold,
                    timestamp = alert.timestamp,
                    "DLQ alert triggered"
                );
            }
        });
    }

    /// Starts the maintenance task for cleanup and monitoring
    fn start_maintenance_task(&self) {
        let messages_by_topic = self.messages_by_topic.clone();
        let policies = Arc::clone(&self.policies);
        let stats = DlqStatsSnapshot {
            total_messages: self.stats.total_messages.load(Ordering::Relaxed),
            messages_recovered: self.stats.messages_recovered.load(Ordering::Relaxed),
            messages_purged: self.stats.messages_purged.load(Ordering::Relaxed),
            alerts_sent: self.stats.alerts_sent.load(Ordering::Relaxed),
            current_size: self.stats.current_size.load(Ordering::Relaxed),
            max_size_reached: self.stats.max_size_reached.load(Ordering::Relaxed),
        };

        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(60)); // Run every minute
            
            loop {
                interval.tick().await;
                
                // Purge expired messages for all topics
                for entry in messages_by_topic.iter() {
                    let topic = entry.key();
                    let policy = policies
                        .read()
                        .get(topic)
                        .cloned()
                        .unwrap_or_else(DlqPolicy::default);

                    if policy.auto_purge_expired {
                        let mut messages = entry.value().clone();
                        let original_len = messages.len();
                        messages.retain(|msg| !msg.is_expired(policy.retention_duration_ms));
                        let purged_count = (original_len - messages.len()) as u64;
                        
                        if purged_count > 0 {
                            // Update the actual messages
                            if let Some(mut topic_messages) = messages_by_topic.get_mut(topic) {
                                topic_messages.retain(|msg| !msg.is_expired(policy.retention_duration_ms));
                            }
                            
                            // Log purge operation
                            // Note: in real implementation, would update stats via separate mechanism
                            debug!("Auto-purged {} expired messages from DLQ for topic {}", purged_count, topic);
                        }
                    }
                }
            }
        });
    }
}

/// Extracts message ID from wire message (similar to qos.rs)
fn extract_message_id(message: &WireMessage) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    
    let mut hasher = DefaultHasher::new();
    message.frame.hash(&mut hasher);
    Ok(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn create_test_wire_message(id: u64) -> Arc<WireMessage> {
        Arc::new(WireMessage {
            frame: Bytes::from(format!("test-message-{}", id)),
            expire_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64 + 60000,
        })
    }

    fn create_test_dlq_message(id: u64, topic: &str, reason: &str) -> DeadLetterMessage {
        DeadLetterMessage::new(
            create_test_wire_message(id),
            topic.to_string(),
            "subscriber1".to_string(),
            reason.to_string(),
            3,
            Vec::new(),
        )
    }

    #[tokio::test]
    async fn test_dlq_basic_operations() {
        let dlq = DeadLetterQueue::new(None);
        
        // Add a message
        let dlq_message = create_test_dlq_message(1, "test-topic", "network error");
        dlq.add_message(dlq_message.clone()).await.unwrap();
        
        // Check statistics
        let stats = dlq.stats();
        assert_eq!(stats.total_messages, 1);
        assert_eq!(stats.current_size, 1);
        
        // List messages
        let messages = dlq.list_messages("test-topic", None);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].failure_category, FailureCategory::NetworkError);
    }

    #[tokio::test]
    async fn test_dlq_recovery() {
        let dlq = DeadLetterQueue::new(None);
        
        // Add a message
        let dlq_message = create_test_dlq_message(1, "test-topic", "temporary error");
        dlq.add_message(dlq_message).await.unwrap();
        
        // Recover the message
        let recovered = dlq.recover_message("test-topic", 0).await.unwrap();
        assert!(recovered.is_some());
        
        // Check it's removed
        let messages = dlq.list_messages("test-topic", None);
        assert_eq!(messages.len(), 0);
        
        let stats = dlq.stats();
        assert_eq!(stats.messages_recovered, 1);
        assert_eq!(stats.current_size, 0);
    }

    #[tokio::test]
    async fn test_dlq_capacity_management() {
        let dlq = DeadLetterQueue::new(None);
        
        // Set a small capacity policy
        let mut policy = DlqPolicy::default();
        policy.max_dlq_size = 2;
        dlq.set_policy("test-topic".to_string(), policy);
        
        // Add messages up to capacity
        for i in 0..3 {
            let dlq_message = create_test_dlq_message(i, "test-topic", "error");
            dlq.add_message(dlq_message).await.unwrap();
        }
        
        // Should only have 2 messages due to capacity limit
        let messages = dlq.list_messages("test-topic", None);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn test_failure_categorization() {
        assert_eq!(FailureCategory::from("network timeout"), FailureCategory::NetworkError);
        assert_eq!(FailureCategory::from("invalid format"), FailureCategory::FormatError);
        assert_eq!(FailureCategory::from("processing failed"), FailureCategory::ProcessingError);
        assert_eq!(FailureCategory::from("auth denied"), FailureCategory::AuthError);
        assert_eq!(FailureCategory::from("out of memory"), FailureCategory::ResourceError);
        assert_eq!(FailureCategory::from("request timeout"), FailureCategory::TimeoutError);
        assert_eq!(FailureCategory::from("unknown issue"), FailureCategory::Unknown);
    }

    #[test]
    fn test_message_expiration() {
        let message = create_test_dlq_message(1, "test-topic", "error");
        
        // Should not be expired with long retention (1 hour = 3600000ms)
        assert!(!message.is_expired(3600000));
        
        // Should not be expired immediately with short retention
        // (the message was just created, so elapsed time should be ~0)
        assert!(!message.is_expired(1000));
        
        // Test with a message that has an older entry time
        use std::time::{SystemTime, UNIX_EPOCH};
        let old_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64 - 5000; // 5 seconds ago
        
        let mut old_message = create_test_dlq_message(2, "test-topic", "old error");
        // Manually set old entry time (this is a test hack)
        old_message.dlq_entry_time = old_time;
        
        // Should be expired with retention less than 5 seconds (5000ms)
        assert!(old_message.is_expired(1000)); // 1 second retention
        assert!(!old_message.is_expired(10000)); // 10 second retention
    }

    #[tokio::test]
    async fn test_purge_operations() {
        let dlq = DeadLetterQueue::new(None);
        
        // Add some messages
        for i in 0..5 {
            let dlq_message = create_test_dlq_message(i, "test-topic", "error");
            dlq.add_message(dlq_message).await.unwrap();
        }
        
        // Purge all
        let purged = dlq.purge_all_messages("test-topic").await.unwrap();
        assert_eq!(purged, 5);
        
        let messages = dlq.list_messages("test-topic", None);
        assert_eq!(messages.len(), 0);
        
        let stats = dlq.stats();
        assert_eq!(stats.messages_purged, 5);
        assert_eq!(stats.current_size, 0);
    }
}