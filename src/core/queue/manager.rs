use dashmap::DashMap;
use std::sync::Arc;
use crossbeam_channel::Sender;

use crate::core::message::Message;
use crate::core::queue::{qos0, QueueBehavior};

/// A thread-safe manager for per-subscriber queues.
///
/// Uses `crossbeam_channel::Sender` for QoS 0 queues (non-blocking, high-throughput).
#[derive(Debug, Default)]
pub struct QueueManager {
    queues: DashMap<String, Arc<dyn QueueBehavior>>,
}

impl QueueManager {
    #[inline]
    pub fn new() -> Self {
        Self {
            queues: DashMap::new(),
        }
    }

    /// Gets or creates a queue for a given subscriber ID using a crossbeam sender.
    ///
    /// If a queue already exists for the subscriber, returns it.
    pub fn get_or_create(
        &self,
        subscriber_id: &str,
        sender: Sender<Arc<Message>>,
    ) -> Arc<dyn QueueBehavior> {
        if let Some(existing) = self.queues.get(subscriber_id) {
            return Arc::clone(&*existing);
        }

        let queue: Arc<dyn QueueBehavior> =
            Arc::new(qos0::Queue::new(subscriber_id.to_string(), sender));

        let entry = self
            .queues
            .entry(subscriber_id.to_string())
            .or_insert_with(|| Arc::clone(&queue));

        Arc::clone(&*entry)
    }

    #[inline]
    pub fn count(&self) -> usize {
        self.queues.len()
    }

    #[inline]
    pub fn remove(&self, subscriber_id: &str) {
        self.queues.remove(subscriber_id);
    }
}
