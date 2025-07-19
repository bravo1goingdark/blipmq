//! QueueManager handles per-subscriber queues for different QoS levels.
//!
//! Uses DashMap for lock-free concurrent access and minimal latency,
//! making it suitable for high-throughput workloads with thousands of subscribers.

use std::sync::Arc;
use dashmap::DashMap;
use tokio::sync::mpsc::UnboundedSender;

use crate::core::message::Message;
use crate::core::queue::{qos0, QueueBehavior};

/// A thread-safe manager for per-subscriber queues.
///
/// Supports pluggable queue backends by using the `QueueBehavior` trait object.
/// Currently defaults to QoS 0 (fire-and-forget).
#[derive(Debug, Default)]
pub struct QueueManager {
    /// Map of subscriber ID → queue.
    queues: DashMap<String, Arc<dyn QueueBehavior>>,
}

impl QueueManager {
    /// Creates a new, empty `QueueManager`.
    #[inline]
    pub fn new() -> Self {
        Self {
            queues: DashMap::new(),
        }
    }

    /// Returns the queue for a subscriber, or creates a new QoS 0 queue if not present.
    ///
    /// # Arguments
    /// - `subscriber_id`: Unique identifier for the subscriber.
    /// - `sender`: The channel used to deliver messages to this subscriber.
    ///
    /// # Returns
    /// A thread-safe reference to the subscriber's queue.
    pub fn get_or_create(
        &self,
        subscriber_id: &str,
        sender: UnboundedSender<Arc<Message>>,
    ) -> Arc<dyn QueueBehavior> {
        if let Some(existing) = self.queues.get(subscriber_id) {
            return Arc::clone(&*existing);
        }

        let queue: Arc<dyn QueueBehavior> =
            Arc::new(qos0::Queue::new(subscriber_id.to_string(), sender));

        // Insert only if not already present (handles race)
        let entry = self
            .queues
            .entry(subscriber_id.to_string())
            .or_insert_with(|| Arc::clone(&queue));

        Arc::clone(&*entry)
    }

    /// Returns the total number of queues currently active.
    #[inline]
    pub fn count(&self) -> usize {
        self.queues.len()
    }

    /// Removes a subscriber queue by ID.
    #[inline]
    pub fn remove(&self, subscriber_id: &str) {
        self.queues.remove(subscriber_id);
    }
}
