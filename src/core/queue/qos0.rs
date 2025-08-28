use crate::core::error::BlipError;
use crate::core::message::Message;
use crate::core::queue::QueueBehavior;
use crossbeam_queue::ArrayQueue;
use std::sync::Arc;
// Removed `std::thread` and `std::time::Duration` as `Block` policy is being removed.

use serde::Deserialize;

/// Defines how a QoS0 queue handles overflow.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverflowPolicy {
    DropNew,
    DropOldest,
    // Removed Block policy for async compatibility and performance.
}

impl Default for OverflowPolicy {
    fn default() -> Self {
        OverflowPolicy::DropOldest
    }
}

/// A single-subscriber, lock-free queue for QoS 0 delivery.
///
/// Internally uses `crossbeam::ArrayQueue` for fast, bounded SPSC communication.
/// Each subscriber gets its own instance of this queue.
#[derive(Debug)]
pub struct Queue {
    name: String,
    queue: Arc<ArrayQueue<Arc<Message>>>,
    policy: OverflowPolicy,
}

impl Queue {
    /// Creates a new `Queue` with a given name, capacity, and overflow policy.
    pub fn new(name: impl Into<String>, capacity: usize, policy: OverflowPolicy) -> Self {
        Self {
            name: name.into(),
            queue: Arc::new(ArrayQueue::new(capacity)),
            policy,
        }
    }

    /// Returns a cloneable reference to the internal `ArrayQueue`.
    pub fn queue(&self) -> Arc<ArrayQueue<Arc<Message>>> {
        Arc::clone(&self.queue)
    }

    /// Drains up to `max` messages for batch flushing.
    pub fn drain(&self, max: usize) -> Vec<Arc<Message>> {
        let mut drained = Vec::with_capacity(max);
        for _ in 0..max {
            if let Some(msg) = self.queue.pop() {
                drained.push(msg);
            } else {
                break;
            }
        }
        drained
    }
}

impl QueueBehavior for Queue {
    fn enqueue(&self, message: Arc<Message>) -> Result<(), BlipError> {
        match self.queue.push(message.clone()) {
            Ok(_) => Ok(()),
            Err(_) => match self.policy {
                OverflowPolicy::DropNew => {
                    // Optional: record a metric here
                    Err(BlipError::QueueFull)
                }
                OverflowPolicy::DropOldest => {
                    // Drop the oldest message and try to enqueue the new one.
                    // This isn't strictly atomic, but `ArrayQueue` is lock-free.
                    // If the queue is still full after popping, it implies contention
                    // or a very small queue where even after popping, it's immediately filled again.
                    // For now, this is a reasonable best-effort.
                    let _ = self.queue.pop(); // Attempt to make space
                    self.queue.push(message).map_err(|_| BlipError::QueueFull)
                }
                // Removed OverflowPolicy::Block
            },
        }
    }

    fn dequeue(&self) -> Option<Arc<Message>> {
        self.queue.pop()
    }

    fn len(&self) -> usize {
        self.queue.len()
    }

    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    fn name(&self) -> &str {
        &self.name
    }
}
