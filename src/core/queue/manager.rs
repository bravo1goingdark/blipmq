use crate::config::CONFIG;
use crate::core::queue::QueueBehavior;
use dashmap::DashMap;
use std::sync::Arc;

pub enum QoS {
    QoS0,
    QoS1,
}

#[derive(Debug, Default)]
pub struct QueueManager {
    queues: DashMap<String, Arc<dyn QueueBehavior + Send + Sync>>,
}

impl QueueManager {
    pub fn new() -> Self {
        Self {
            queues: DashMap::new(),
        }
    }

    /// Get or create a queue for this subscriber with the given QoS and capacity.
    /// Note: Currently, QoS1 is not fully implemented and will default to QoS0 behavior.
    pub fn get_or_create(
        &self,
        subscriber_id: impl Into<String>,
        qos: QoS,
        capacity: usize,
    ) -> Arc<dyn QueueBehavior + Send + Sync> {
        let key = subscriber_id.into();

        // entry closure captures `qos`, `capacity`, and `key` by move
        let entry = self.queues.entry(key.clone()).or_insert_with(|| {
            // Build the concrete queue based on QoS
            let queue: Arc<dyn QueueBehavior + Send + Sync> = match qos {
                QoS::QoS0 => {
                    let q = crate::core::queue::qos0::Queue::new(
                        key.clone(),
                        capacity,
                        CONFIG.queues.overflow_policy,
                    );
                    Arc::new(q)
                }
                QoS::QoS1 => {
                    // TODO: QoS1 is not yet implemented. Falling back to QoS0.
                    // This should be replaced with a proper QoS1 queue implementation.
                    let q = crate::core::queue::qos0::Queue::new(
                        key.clone(),
                        capacity,
                        CONFIG.queues.overflow_policy,
                    );
                    Arc::new(q)
                }
            };
            queue
        });

        Arc::clone(&*entry)
    }

    #[inline]
    pub fn remove(&self, subscriber_id: &str) {
        self.queues.remove(subscriber_id);
    }

    #[inline]
    pub fn count(&self) -> usize {
        self.queues.len()
    }
}
