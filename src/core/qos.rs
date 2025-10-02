//! Quality of Service (QoS) and Message Acknowledgments
//!
//! Implements different delivery guarantee levels:
//! - QoS 0: At most once (fire and forget)
//! - QoS 1: At least once (acknowledged delivery)
//! - QoS 2: Exactly once (guaranteed delivery)
//!
//! Features:
//! - Message acknowledgment tracking
//! - Retry mechanisms with exponential backoff
//! - Duplicate detection and deduplication
//! - Persistence for reliable delivery

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::{interval, sleep, Instant};
use tracing::{debug, error, info, warn};

use crate::core::message::{Message, WireMessage};
use crate::core::wal::{WalEntryType, WriteAheadLog};

/// Quality of Service levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum QosLevel {
    /// At most once - fire and forget
    AtMostOnce = 0,
    /// At least once - acknowledged delivery
    AtLeastOnce = 1,
    /// Exactly once - guaranteed delivery (not implemented in v1)
    ExactlyOnce = 2,
}

impl From<u8> for QosLevel {
    fn from(level: u8) -> Self {
        match level {
            0 => QosLevel::AtMostOnce,
            1 => QosLevel::AtLeastOnce,
            2 => QosLevel::ExactlyOnce,
            _ => QosLevel::AtMostOnce, // Default to most basic
        }
    }
}

impl From<QosLevel> for u8 {
    fn from(qos: QosLevel) -> Self {
        qos as u8
    }
}

/// Message acknowledgment types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AckType {
    /// Positive acknowledgment - message processed successfully
    Ack,
    /// Negative acknowledgment - message processing failed
    Nack,
    /// Reject - message rejected, don't retry
    Reject,
}

/// Acknowledgment message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Acknowledgment {
    pub message_id: u64,
    pub subscriber_id: String,
    pub topic: String,
    pub ack_type: AckType,
    pub reason: Option<String>,
    pub timestamp: u64,
}

impl Acknowledgment {
    pub fn new(
        message_id: u64,
        subscriber_id: String,
        topic: String,
        ack_type: AckType,
        reason: Option<String>,
    ) -> Self {
        Self {
            message_id,
            subscriber_id,
            topic,
            ack_type,
            reason,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        }
    }
}

/// Pending message awaiting acknowledgment
#[derive(Debug, Clone)]
pub struct PendingMessage {
    pub message: Arc<WireMessage>,
    pub subscriber_id: String,
    pub topic: String,
    pub qos_level: QosLevel,
    pub sent_at: u64,
    pub retry_count: u32,
    pub max_retries: u32,
    pub retry_interval_ms: u64,
    pub next_retry_at: u64,
}

impl PendingMessage {
    pub fn new(
        message: Arc<WireMessage>,
        subscriber_id: String,
        topic: String,
        qos_level: QosLevel,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        Self {
            message,
            subscriber_id,
            topic,
            qos_level,
            sent_at: now,
            retry_count: 0,
            max_retries: 3,
            retry_interval_ms: 1000, // 1 second initial retry
            next_retry_at: now + 1000,
        }
    }

    pub fn should_retry(&self, now: u64) -> bool {
        self.retry_count < self.max_retries && now >= self.next_retry_at
    }

    pub fn increment_retry(&mut self) {
        self.retry_count += 1;
        // Exponential backoff with jitter
        let backoff_ms = self.retry_interval_ms * (1 << self.retry_count) as u64;
        let jitter = (rand::random::<u64>() % 1000).min(backoff_ms / 10);
        
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
            
        self.next_retry_at = now + backoff_ms + jitter;
    }

    pub fn is_expired(&self, now: u64, timeout_ms: u64) -> bool {
        now - self.sent_at > timeout_ms
    }
}

/// Message deduplication manager
#[derive(Debug, Default, Clone)]
pub struct DeduplicationManager {
    /// Recently seen message IDs per subscriber
    seen_messages: DashMap<String, HashSet<u64>>,
    /// Cleanup interval
    cleanup_interval_ms: u64,
}

impl DeduplicationManager {
    pub fn new(cleanup_interval_ms: u64) -> Self {
        Self {
            seen_messages: DashMap::new(),
            cleanup_interval_ms,
        }
    }

    pub fn is_duplicate(&self, subscriber_id: &str, message_id: u64) -> bool {
        self.seen_messages
            .get(subscriber_id)
            .map(|set| set.contains(&message_id))
            .unwrap_or(false)
    }

    pub fn record_message(&self, subscriber_id: &str, message_id: u64) {
        self.seen_messages
            .entry(subscriber_id.to_string())
            .or_insert_with(HashSet::new)
            .insert(message_id);
    }

    pub fn cleanup_old_entries(&self) {
        // In production, this would include timestamp-based cleanup
        // For now, we limit the size per subscriber
        for mut entry in self.seen_messages.iter_mut() {
            if entry.len() > 10000 {
                // Keep only the most recent 5000 entries
                let mut ids: Vec<u64> = entry.iter().cloned().collect();
                ids.sort_unstable();
                let keep_from = ids.len().saturating_sub(5000);
                
                entry.clear();
                entry.extend(ids.into_iter().skip(keep_from));
            }
        }
    }
}

/// QoS Manager handles acknowledgments and delivery guarantees
pub struct QosManager {
    /// Pending messages awaiting acknowledgment (message_id -> PendingMessage)
    pending_messages: Arc<DashMap<u64, PendingMessage>>,
    
    /// Acknowledgments received (message_id -> Acknowledgment)
    acknowledgments: Arc<DashMap<u64, Acknowledgment>>,
    
    /// Deduplication manager
    dedup_manager: DeduplicationManager,
    
    /// Write-ahead log for durability
    wal: Option<Arc<WriteAheadLog>>,
    
    /// Statistics
    stats: Arc<QosStats>,
    
    /// Configuration
    config: QosConfig,
    
    /// Acknowledgment channel
    ack_tx: mpsc::UnboundedSender<Acknowledgment>,
}

/// QoS configuration
#[derive(Debug, Clone)]
pub struct QosConfig {
    pub ack_timeout_ms: u64,
    pub max_pending_messages: usize,
    pub retry_interval_ms: u64,
    pub max_retries: u32,
    pub cleanup_interval_ms: u64,
    pub enable_deduplication: bool,
    pub persist_pending_messages: bool,
}

impl Default for QosConfig {
    fn default() -> Self {
        Self {
            ack_timeout_ms: 30_000,        // 30 seconds
            max_pending_messages: 100_000, // 100k pending messages
            retry_interval_ms: 1_000,      // 1 second base retry interval
            max_retries: 3,
            cleanup_interval_ms: 60_000,   // 1 minute cleanup
            enable_deduplication: true,
            persist_pending_messages: true,
        }
    }
}

/// QoS statistics
#[derive(Debug, Default)]
pub struct QosStats {
    pub messages_sent: AtomicU64,
    pub messages_acked: AtomicU64,
    pub messages_nacked: AtomicU64,
    pub messages_rejected: AtomicU64,
    pub messages_retried: AtomicU64,
    pub messages_expired: AtomicU64,
    pub duplicates_filtered: AtomicU64,
    pub pending_count: AtomicU64,
}

/// Serializable snapshot of QoS statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QosStatsSnapshot {
    pub messages_sent: u64,
    pub messages_acked: u64,
    pub messages_nacked: u64,
    pub messages_rejected: u64,
    pub messages_retried: u64,
    pub messages_expired: u64,
    pub duplicates_filtered: u64,
    pub pending_count: u64,
}

impl QosStats {
    pub fn snapshot(&self) -> QosStatsSnapshot {
        QosStatsSnapshot {
            messages_sent: self.messages_sent.load(Ordering::Relaxed),
            messages_acked: self.messages_acked.load(Ordering::Relaxed),
            messages_nacked: self.messages_nacked.load(Ordering::Relaxed),
            messages_rejected: self.messages_rejected.load(Ordering::Relaxed),
            messages_retried: self.messages_retried.load(Ordering::Relaxed),
            messages_expired: self.messages_expired.load(Ordering::Relaxed),
            duplicates_filtered: self.duplicates_filtered.load(Ordering::Relaxed),
            pending_count: self.pending_count.load(Ordering::Relaxed),
        }
    }
}


impl QosManager {
    pub async fn new(config: QosConfig, wal: Option<Arc<WriteAheadLog>>) -> Self {
        let (ack_tx, ack_rx) = mpsc::unbounded_channel();

        let qos_manager = Self {
            pending_messages: Arc::new(DashMap::new()),
            acknowledgments: Arc::new(DashMap::new()),
            dedup_manager: DeduplicationManager::new(config.cleanup_interval_ms),
            wal: wal.clone(),
            stats: Arc::new(QosStats::default()),
            config: config.clone(),
            ack_tx,
        };

        // Start background tasks
        qos_manager.start_ack_processor(ack_rx).await;
        qos_manager.start_retry_processor().await;
        qos_manager.start_cleanup_task().await;

        qos_manager
    }

    /// Sends a message with the specified QoS level
    pub async fn send_message(
        &self,
        message: Arc<WireMessage>,
        subscriber_id: String,
        topic: String,
        qos_level: QosLevel,
    ) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        let message_id = extract_message_id(&message)?;

        // Check for duplicates if deduplication is enabled
        if self.config.enable_deduplication && qos_level != QosLevel::AtMostOnce {
            if self.dedup_manager.is_duplicate(&subscriber_id, message_id) {
                self.stats.duplicates_filtered.fetch_add(1, Ordering::Relaxed);
                debug!("Filtered duplicate message {} for subscriber {}", message_id, subscriber_id);
                return Ok(message_id);
            }
            self.dedup_manager.record_message(&subscriber_id, message_id);
        }

        match qos_level {
            QosLevel::AtMostOnce => {
                // Fire and forget - no acknowledgment tracking
                self.deliver_message_immediately(message, &subscriber_id, &topic).await?;
            }
            QosLevel::AtLeastOnce => {
                // Track message for acknowledgment
                let pending_msg = PendingMessage::new(
                    message.clone(),
                    subscriber_id.clone(),
                    topic.clone(),
                    qos_level,
                );

                // Store in pending messages
                self.pending_messages.insert(message_id, pending_msg);
                self.stats.pending_count.fetch_add(1, Ordering::Relaxed);

                // Persist to WAL if enabled
                if self.config.persist_pending_messages && self.wal.is_some() {
                    let wal = self.wal.as_ref().unwrap();
                    wal.write_entry(WalEntryType::MessagePublished {
                        topic: topic.clone(),
                        message_id,
                        payload: message.frame.clone(),
                        timestamp: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64,
                        ttl_ms: 0, // Will be calculated from message
                    }).await?;
                }

                // Deliver the message
                self.deliver_message_immediately(message, &subscriber_id, &topic).await?;
            }
            QosLevel::ExactlyOnce => {
                // Not implemented in v1 - fallback to at-least-once
                warn!("QoS 2 (exactly once) not implemented, falling back to at-least-once");
                return Box::pin(self.send_message(message, subscriber_id, topic, QosLevel::AtLeastOnce)).await;
            }
        }

        self.stats.messages_sent.fetch_add(1, Ordering::Relaxed);
        Ok(message_id)
    }

    /// Processes an acknowledgment
    pub async fn process_acknowledgment(&self, ack: Acknowledgment) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.ack_tx.send(ack)?;
        Ok(())
    }

    /// Delivers a message immediately (actual delivery implementation would be here)
    async fn deliver_message_immediately(
        &self,
        message: Arc<WireMessage>,
        subscriber_id: &str,
        topic: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // This is where the actual message delivery would happen
        // In the real implementation, this would interface with the network layer
        debug!("Delivering message to subscriber {} on topic {}", subscriber_id, topic);
        Ok(())
    }

    /// Starts the acknowledgment processor
    async fn start_ack_processor(&self, mut ack_rx: mpsc::UnboundedReceiver<Acknowledgment>) {
        let pending_messages = Arc::clone(&self.pending_messages);
        let acknowledgments = Arc::clone(&self.acknowledgments);
        let stats = Arc::clone(&self.stats);
        let wal = self.wal.clone();

        tokio::spawn(async move {
            while let Some(ack) = ack_rx.recv().await {
                let message_id = ack.message_id;

                // Store the acknowledgment
                acknowledgments.insert(message_id, ack.clone());

                // Process based on ack type
                match ack.ack_type {
                    AckType::Ack => {
                        // Remove from pending messages
                        if let Some((_, _)) = pending_messages.remove(&message_id) {
                            stats.pending_count.fetch_sub(1, Ordering::Relaxed);
                            stats.messages_acked.fetch_add(1, Ordering::Relaxed);
                            
                            debug!("Message {} acknowledged by {}", message_id, ack.subscriber_id);

                            // Log to WAL if available
                            if let Some(ref wal) = wal {
                                if let Err(e) = wal.write_entry(WalEntryType::MessageAcknowledged {
                                    topic: ack.topic.clone(),
                                    message_id,
                                    subscriber_id: ack.subscriber_id.clone(),
                                }).await {
                                    error!("Failed to log acknowledgment to WAL: {}", e);
                                }
                            }
                        }
                    }
                    AckType::Nack => {
                        stats.messages_nacked.fetch_add(1, Ordering::Relaxed);
                        debug!("Message {} nacked by {}: {:?}", message_id, ack.subscriber_id, ack.reason);
                        // Message will be retried by the retry processor
                    }
                    AckType::Reject => {
                        // Remove from pending messages - no retry
                        if let Some((_, _)) = pending_messages.remove(&message_id) {
                            stats.pending_count.fetch_sub(1, Ordering::Relaxed);
                            stats.messages_rejected.fetch_add(1, Ordering::Relaxed);
                            
                            warn!("Message {} rejected by {}: {:?}", message_id, ack.subscriber_id, ack.reason);
                        }
                    }
                }
            }
        });
    }

    /// Starts the retry processor
    async fn start_retry_processor(&self) {
        let pending_messages = Arc::clone(&self.pending_messages);
        let config = self.config.clone();
        let stats = Arc::clone(&self.stats);
        
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_millis(config.retry_interval_ms));
            
            loop {
                interval.tick().await;
                
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                
                let mut to_retry = Vec::new();
                let mut to_expire = Vec::new();
                
                // Check all pending messages
                for entry in pending_messages.iter() {
                    let message_id = *entry.key();
                    let pending_msg = entry.value();
                    
                    if pending_msg.is_expired(now, config.ack_timeout_ms) {
                        to_expire.push(message_id);
                    } else if pending_msg.should_retry(now) {
                        to_retry.push(message_id);
                    }
                }
                
                // Process retries
                for message_id in to_retry {
                    if let Some(mut pending_msg) = pending_messages.get_mut(&message_id) {
                        pending_msg.increment_retry();
                        stats.messages_retried.fetch_add(1, Ordering::Relaxed);
                        
                        debug!("Retrying message {} (attempt {}/{})", 
                               message_id, pending_msg.retry_count, pending_msg.max_retries);
                        
                        // In real implementation, would trigger redelivery
                    }
                }
                
                // Process expired messages
                for message_id in to_expire {
                    if let Some((_, _)) = pending_messages.remove(&message_id) {
                        stats.pending_count.fetch_sub(1, Ordering::Relaxed);
                        stats.messages_expired.fetch_add(1, Ordering::Relaxed);
                        
                        warn!("Message {} expired after {} ms", message_id, config.ack_timeout_ms);
                    }
                }
            }
        });
    }

    /// Starts the cleanup task
    async fn start_cleanup_task(&self) {
        let acknowledgments = Arc::clone(&self.acknowledgments);
        let dedup_manager = Arc::new(self.dedup_manager.clone());
        let cleanup_interval = Duration::from_millis(self.config.cleanup_interval_ms);
        
        tokio::spawn(async move {
            let mut interval = interval(cleanup_interval);
            
            loop {
                interval.tick().await;
                
                // Clean up old acknowledgments (keep last 1 hour)
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                
                acknowledgments.retain(|_, ack| {
                    now - ack.timestamp < 3600_000 // 1 hour
                });
                
                // Clean up deduplication entries
                dedup_manager.cleanup_old_entries();
                
                debug!("Cleaned up old acknowledgments and deduplication entries");
            }
        });
    }

    /// Returns current statistics
    pub fn stats(&self) -> QosStatsSnapshot {
        self.stats.snapshot()
    }

    /// Returns the number of pending messages
    pub fn pending_count(&self) -> usize {
        self.pending_messages.len()
    }

    /// Gets acknowledgment for a message
    pub fn get_acknowledgment(&self, message_id: u64) -> Option<Acknowledgment> {
        self.acknowledgments.get(&message_id).map(|ack| ack.clone())
    }

    /// Checks if a message is pending
    pub fn is_message_pending(&self, message_id: u64) -> bool {
        self.pending_messages.contains_key(&message_id)
    }
}

/// Extracts message ID from wire message
fn extract_message_id(message: &WireMessage) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    // In a real implementation, this would parse the message frame
    // For now, we'll use a hash of the frame as the message ID
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

    fn create_test_message(id: u64) -> Arc<WireMessage> {
        Arc::new(WireMessage {
            frame: Bytes::from(format!("test-message-{}", id)),
            expire_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64 + 60000, // 1 minute TTL
        })
    }

    #[tokio::test]
    async fn test_qos_at_most_once() {
        let config = QosConfig::default();
        let qos_manager = QosManager::new(config, None).await;
        
        let message = create_test_message(123);
        let result = qos_manager.send_message(
            message,
            "subscriber1".to_string(),
            "test-topic".to_string(),
            QosLevel::AtMostOnce,
        ).await;
        
        assert!(result.is_ok());
        
        // Should have no pending messages for QoS 0
        assert_eq!(qos_manager.pending_count(), 0);
        
        let stats = qos_manager.stats();
        assert_eq!(stats.messages_sent, 1);
    }

    #[tokio::test]
    async fn test_qos_at_least_once() {
        let config = QosConfig::default();
        let qos_manager = QosManager::new(config, None).await;
        
        let message = create_test_message(123);
        let message_id = qos_manager.send_message(
            message,
            "subscriber1".to_string(),
            "test-topic".to_string(),
            QosLevel::AtLeastOnce,
        ).await.unwrap();
        
        // Should have one pending message
        assert_eq!(qos_manager.pending_count(), 1);
        assert!(qos_manager.is_message_pending(message_id));
        
        // Send acknowledgment
        let ack = Acknowledgment::new(
            message_id,
            "subscriber1".to_string(),
            "test-topic".to_string(),
            AckType::Ack,
            None,
        );
        
        qos_manager.process_acknowledgment(ack).await.unwrap();
        
        // Wait for ack processing
        sleep(Duration::from_millis(100)).await;
        
        // Should be removed from pending
        assert_eq!(qos_manager.pending_count(), 0);
        
        let stats = qos_manager.stats();
        assert_eq!(stats.messages_sent, 1);
        assert_eq!(stats.messages_acked, 1);
    }

    #[tokio::test]
    async fn test_deduplication() {
        let config = QosConfig::default();
        let qos_manager = QosManager::new(config, None).await;
        
        let message = create_test_message(123);
        
        // Send same message twice
        let message_id1 = qos_manager.send_message(
            message.clone(),
            "subscriber1".to_string(),
            "test-topic".to_string(),
            QosLevel::AtLeastOnce,
        ).await.unwrap();
        
        let message_id2 = qos_manager.send_message(
            message,
            "subscriber1".to_string(),
            "test-topic".to_string(),
            QosLevel::AtLeastOnce,
        ).await.unwrap();
        
        assert_eq!(message_id1, message_id2);
        
        // Should only have one pending message due to deduplication
        assert_eq!(qos_manager.pending_count(), 1);
        
        let stats = qos_manager.stats();
        assert_eq!(stats.duplicates_filtered, 1);
    }

    #[test]
    fn test_acknowledgment_types() {
        let ack = Acknowledgment::new(
            123,
            "subscriber1".to_string(),
            "test-topic".to_string(),
            AckType::Ack,
            None,
        );
        
        assert_eq!(ack.message_id, 123);
        assert_eq!(ack.ack_type, AckType::Ack);
        assert_eq!(ack.subscriber_id, "subscriber1");
        assert!(ack.timestamp > 0);
    }

    #[test]
    fn test_pending_message_retry_logic() {
        let message = create_test_message(123);
        let mut pending = PendingMessage::new(
            message,
            "subscriber1".to_string(),
            "test-topic".to_string(),
            QosLevel::AtLeastOnce,
        );
        
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        
        // Should not retry immediately
        assert!(!pending.should_retry(now));
        
        // Should retry after interval
        assert!(pending.should_retry(now + 2000));
        
        // Increment retry count
        pending.increment_retry();
        assert_eq!(pending.retry_count, 1);
        assert!(pending.next_retry_at > now);
    }
}