use crate::core::error::BlipError;
use crate::core::message::Message;
use crate::core::queue::QueueBehavior;
use crossbeam_queue::ArrayQueue;
use std::sync::Arc;

/// A single-subscriber, lock-free queue for QoS 0 delivery.
///
/// Internally uses `crossbeam::ArrayQueue` for fast, bounded SPSC communication.
/// Each subscriber gets its own instance of this queue.
#[derive(Debug)]
pub struct Queue {
    name: String,
    queue: Arc<ArrayQueue<Arc<Message>>>,
}

impl Queue {
    /// Creates a new `Queue` with a given name and fixed capacity.
    ///
    /// # Arguments
    ///
    /// * `name` - A human-readable identifier for the queue (used in metrics/logging).
    /// * `capacity` - Maximum number of messages this queue can hold.
    ///
    /// # Returns
    ///
    /// A new `Queue` wrapped around a lock-free bounded ring buffer.
    pub fn new(name: impl Into<String>, capacity: usize) -> Self {
        Self {
            name: name.into(),
            queue: Arc::new(ArrayQueue::new(capacity)),
        }
    }

    /// Returns a cloneable reference to the internal `ArrayQueue`.
    ///
    /// This is used by subscriber flush tasks to drain messages.
    pub fn queue(&self) -> Arc<ArrayQueue<Arc<Message>>> {
        Arc::clone(&self.queue)
    }
}

impl QueueBehavior for Queue {
    /// Pushes a message into the queue.
    ///
    /// # Errors
    ///
    /// Returns `BlipError::QueueFull` if the queue is at capacity.
    fn enqueue(&self, message: Arc<Message>) -> Result<(), BlipError> {
        self.queue.push(message).map_err(|_| BlipError::QueueFull)
    }

    /// Pops a message from the queue (for testing or non-reactive usage).
    fn dequeue(&self) -> Option<Arc<Message>> {
        if self.queue.is_empty() {
            None
        } else {
            // Safe: we've checked it isn't empty
            Some(self.queue.pop()?)
        }
    }

    /// Returns the number of messages currently in the queue.
    fn len(&self) -> usize {
        self.queue.len()
    }

    /// Returns `true` if the queue is empty.
    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Returns the name of the queue.
    fn name(&self) -> &str {
        &self.name
    }
}
