use crate::core::error::BlipError;
use crate::core::message::Message;
use crate::core::queue::QueueBehavior;
use crossbeam_queue::ArrayQueue;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde::Deserialize;

/// Defines how a QoS0 queue handles overflow.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverflowPolicy {
    DropNew,
    DropOldest,
    Block,
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
                    // Drop one and try again
                    let _ = self.queue.pop();
                    self.queue.push(message).map_err(|_| BlipError::QueueFull)
                }
                OverflowPolicy::Block => {
                    // BLOCKING ONLY: caution — not async-compatible
                    // Useful for internal test paths or controlled environments
                    loop {
                        if self.queue.push(message.clone()).is_ok() {
                            return Ok(());
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                }
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
