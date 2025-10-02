//! High-performance batch processing for BlipMQ
//!
//! This module provides batch processing capabilities for various operations
//! to improve throughput and reduce per-message overhead.

use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::time::interval;
use tracing::{debug, trace, warn};

use crate::core::memory_pool::PooledObject;
use crate::core::message::WireMessage;
use crate::core::timer_wheel::TimerManager;

/// Configuration for batch processing
#[derive(Debug, Clone)]
pub struct BatchConfig {
    /// Maximum number of items in a batch
    pub max_batch_size: usize,
    /// Maximum time to wait before processing a partial batch
    pub max_batch_delay_ms: u64,
    /// Channel capacity for batch queues
    pub channel_capacity: usize,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            max_batch_size: 1000,
            max_batch_delay_ms: 10,
            channel_capacity: 8192,
        }
    }
}

/// Statistics for batch processing
#[derive(Debug, Default, Clone)]
pub struct BatchStats {
    pub total_batches: u64,
    pub total_items: u64,
    pub avg_batch_size: f64,
    pub max_batch_size: usize,
    pub flush_by_size: u64,
    pub flush_by_timeout: u64,
    pub processing_time_ms: u64,
}

impl BatchStats {
    fn update(&mut self, batch_size: usize, flushed_by_timeout: bool, processing_time: Duration) {
        self.total_batches += 1;
        self.total_items += batch_size as u64;
        self.avg_batch_size = self.total_items as f64 / self.total_batches as f64;
        self.max_batch_size = self.max_batch_size.max(batch_size);
        
        if flushed_by_timeout {
            self.flush_by_timeout += 1;
        } else {
            self.flush_by_size += 1;
        }
        
        self.processing_time_ms += processing_time.as_millis() as u64;
    }
}

/// Generic batch processor that accumulates items and processes them in batches
pub struct BatchProcessor<T> {
    config: BatchConfig,
    stats: Arc<Mutex<BatchStats>>,
    processor: Arc<dyn Fn(Vec<T>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> + Send + Sync>,
    tx: mpsc::Sender<T>,
}

impl<T> BatchProcessor<T> 
where 
    T: Send + 'static,
{
    /// Creates a new batch processor
    pub fn new<F>(config: BatchConfig, processor: F) -> Self 
    where
        F: Fn(Vec<T>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> + Send + Sync + 'static,
    {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let stats = Arc::new(Mutex::new(BatchStats::default()));
        let processor_arc = Arc::new(processor);
        
        // Spawn batch processing task
        let config_clone = config.clone();
        let stats_clone = Arc::clone(&stats);
        let processor_clone = Arc::clone(&processor_arc);
        
        tokio::spawn(async move {
            Self::batch_processing_task(config_clone, stats_clone, processor_clone, rx).await;
        });
        
        Self {
            config,
            stats,
            processor: processor_arc,
            tx,
        }
    }
    
    /// Submits an item for batch processing
    pub async fn submit(&self, item: T) -> Result<(), mpsc::error::SendError<T>> {
        self.tx.send(item).await
    }
    
    /// Submits an item for batch processing (non-blocking)
    pub fn try_submit(&self, item: T) -> Result<(), mpsc::error::TrySendError<T>> {
        self.tx.try_send(item)
    }
    
    /// Returns current batch processing statistics
    pub fn stats(&self) -> BatchStats {
        self.stats.lock().clone()
    }
    
    async fn batch_processing_task(
        config: BatchConfig,
        stats: Arc<Mutex<BatchStats>>,
        processor: Arc<dyn Fn(Vec<T>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> + Send + Sync>,
        mut rx: mpsc::Receiver<T>,
    ) {
        let mut batch = Vec::with_capacity(config.max_batch_size);
        let mut interval = interval(Duration::from_millis(config.max_batch_delay_ms));
        let mut last_flush = Instant::now();
        
        loop {
            tokio::select! {
                // Receive new item
                item = rx.recv() => {
                    match item {
                        Some(item) => {
                            batch.push(item);
                            
                            // Flush if batch is full
                            if batch.len() >= config.max_batch_size {
                                let start = Instant::now();
                                let batch_size = batch.len();
                                
                                if let Err(e) = processor(std::mem::take(&mut batch)) {
                                    warn!("Batch processing error: {}", e);
                                }
                                
                                let processing_time = start.elapsed();
                                stats.lock().update(batch_size, false, processing_time);
                                last_flush = Instant::now();
                            }
                        }
                        None => {
                            // Channel closed, flush remaining items and exit
                            if !batch.is_empty() {
                                let start = Instant::now();
                                let batch_size = batch.len();
                                
                                if let Err(e) = processor(std::mem::take(&mut batch)) {
                                    warn!("Final batch processing error: {}", e);
                                }
                                
                                let processing_time = start.elapsed();
                                stats.lock().update(batch_size, false, processing_time);
                            }
                            debug!("Batch processor shutting down");
                            break;
                        }
                    }
                }
                
                // Timeout flush
                _ = interval.tick() => {
                    if !batch.is_empty() && last_flush.elapsed() >= Duration::from_millis(config.max_batch_delay_ms) {
                        let start = Instant::now();
                        let batch_size = batch.len();
                        
                        if let Err(e) = processor(std::mem::take(&mut batch)) {
                            warn!("Timeout batch processing error: {}", e);
                        }
                        
                        let processing_time = start.elapsed();
                        stats.lock().update(batch_size, true, processing_time);
                        last_flush = Instant::now();
                    }
                }
            }
        }
    }
}

/// Specialized batch processor for TTL expiration
pub struct TTLBatchProcessor {
    processor: BatchProcessor<Arc<WireMessage>>,
    timer_manager: Arc<TimerManager>,
}

impl TTLBatchProcessor {
    /// Creates a new TTL batch processor
    pub fn new(config: BatchConfig, timer_manager: Arc<TimerManager>) -> Self {
        
        let processor = BatchProcessor::new(config, move |expired_messages| {
            debug!("Processing batch of {} expired messages", expired_messages.len());
            
            // Update metrics for expired messages
            crate::metrics::inc_dropped_ttl(expired_messages.len() as u64);
            
            // Could add additional cleanup logic here
            trace!("Cleaned up {} expired messages", expired_messages.len());
            Ok(())
        });
        
        Self {
            processor,
            timer_manager,
        }
    }
    
    /// Schedules a message for TTL expiration and batch processing
    pub async fn schedule_message(&self, message: Arc<WireMessage>) -> Result<(), String> {
        // Schedule in timer wheel
        if let Err(e) = self.timer_manager.schedule_expiration(message.clone()) {
            return Err(format!("Timer scheduling failed: {}", e));
        }
        
        // For immediate expired messages, add to batch processor
        let now = crate::core::message::current_timestamp();
        if message.is_expired(now) {
            if let Err(_) = self.processor.try_submit(message) {
                warn!("Failed to submit expired message to batch processor");
            }
        }
        
        Ok(())
    }
    
    /// Returns combined statistics
    pub fn stats(&self) -> (BatchStats, (u64, u64, u64)) {
        (self.processor.stats(), self.timer_manager.stats())
    }
}

/// Batch processor for message fanout operations
pub struct FanoutBatchProcessor {
    processor: BatchProcessor<FanoutItem>,
}

#[derive(Debug)]
pub struct FanoutItem {
    pub message: Arc<WireMessage>,
    pub subscriber_count: usize,
}

impl FanoutBatchProcessor {
    /// Creates a new fanout batch processor
    pub fn new(config: BatchConfig) -> Self {
        let processor = BatchProcessor::new(config, |fanout_items| {
            let total_messages = fanout_items.len();
            let total_fanouts: usize = fanout_items.iter().map(|item: &FanoutItem| item.subscriber_count).sum();
            
            debug!("Processing fanout batch: {} messages to {} total subscribers", 
                   total_messages, total_fanouts);
            
            // Update metrics
            crate::metrics::inc_enqueued(total_fanouts as u64);
            
            // Additional fanout processing logic would go here
            Ok(())
        });
        
        Self { processor }
    }
    
    /// Submits a message for batch fanout processing
    pub async fn submit_fanout(&self, message: Arc<WireMessage>, subscriber_count: usize) -> Result<(), String> {
        let item = FanoutItem { message, subscriber_count };
        
        self.processor.submit(item).await
            .map_err(|e| format!("Failed to submit fanout item: {}", e))
    }
    
    /// Returns fanout batch statistics
    pub fn stats(&self) -> BatchStats {
        self.processor.stats()
    }
}

// Note: PooledBatchProcessor removed due to generic type complexity
// In production, you would implement specific pooled processors for each type

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::sleep;
    use bytes::Bytes;
    
    #[tokio::test]
    async fn test_batch_processor_basic() {
        let processed_count = Arc::new(AtomicUsize::new(0));
        let processed_count_clone = Arc::clone(&processed_count);
        
        let config = BatchConfig {
            max_batch_size: 5,
            max_batch_delay_ms: 50,
            channel_capacity: 100,
        };
        
        let processor = BatchProcessor::new(config, move |batch: Vec<i32>| {
            processed_count_clone.fetch_add(batch.len(), Ordering::Relaxed);
            Ok(())
        });
        
        // Submit items
        for i in 0..12 {
            processor.submit(i).await.unwrap();
        }
        
        // Wait for processing
        sleep(Duration::from_millis(100)).await;
        
        assert_eq!(processed_count.load(Ordering::Relaxed), 12);
        
        let stats = processor.stats();
        assert_eq!(stats.total_items, 12);
        assert!(stats.total_batches >= 2); // At least 2 batches (10 items + 2 remaining)
    }
    
    #[tokio::test] 
    async fn test_batch_processor_timeout() {
        let processed_count = Arc::new(AtomicUsize::new(0));
        let processed_count_clone = Arc::clone(&processed_count);
        
        let config = BatchConfig {
            max_batch_size: 100,  // Large batch size
            max_batch_delay_ms: 20, // Short timeout
            channel_capacity: 100,
        };
        
        let processor = BatchProcessor::new(config, move |batch: Vec<i32>| {
            processed_count_clone.fetch_add(batch.len(), Ordering::Relaxed);
            Ok(())
        });
        
        // Submit few items (won't fill batch)
        for i in 0..3 {
            processor.submit(i).await.unwrap();
        }
        
        // Wait for timeout flush
        sleep(Duration::from_millis(50)).await;
        
        assert_eq!(processed_count.load(Ordering::Relaxed), 3);
        
        let stats = processor.stats();
        assert_eq!(stats.flush_by_timeout, 1);
    }
    
    fn create_test_wire_message() -> Arc<WireMessage> {
        Arc::new(WireMessage {
            frame: Bytes::from("test"),
            expire_at: crate::core::message::current_timestamp() + 1000,
        })
    }
    
    #[tokio::test]
    async fn test_ttl_batch_processor() {
        let timer_manager = Arc::new(TimerManager::new());
        let config = BatchConfig {
            max_batch_size: 10,
            max_batch_delay_ms: 20,
            channel_capacity: 100,
        };
        
        let processor = TTLBatchProcessor::new(config, timer_manager);
        
        // Schedule some messages
        for _ in 0..5 {
            let msg = create_test_wire_message();
            processor.schedule_message(msg).await.unwrap();
        }
        
        sleep(Duration::from_millis(50)).await;
        
        let (batch_stats, timer_stats) = processor.stats();
        assert!(timer_stats.0 >= 5); // At least 5 insertions
    }
    
    #[tokio::test] 
    async fn test_fanout_batch_processor() {
        let config = BatchConfig {
            max_batch_size: 3,
            max_batch_delay_ms: 20,
            channel_capacity: 100,
        };
        
        let processor = FanoutBatchProcessor::new(config);
        
        // Submit fanout items
        for i in 1..=5 {
            let msg = create_test_wire_message();
            processor.submit_fanout(msg, i * 10).await.unwrap();
        }
        
        sleep(Duration::from_millis(50)).await;
        
        let stats = processor.stats();
        assert_eq!(stats.total_items, 5);
    }
}