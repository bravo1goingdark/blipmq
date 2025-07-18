use std::sync::Arc;
use tokio::sync::mpsc::Sender;

use crate::core::message::Message;

/// A lightweight, non-blocking queue for QoS 0 message delivery.
/// Messages are dropped silently if the queue is full.
#[derive(Debug, Clone)]
pub struct Queue {
    name: String,
    sender: Sender<Arc<Message>>, // fast, async, non-blocking channel
}
impl Queue {
    /// Creates a new queue with a name and an associated message sender.
    ///
    /// # Arguments
    ///
    /// * `name` - A name for the queue (typically subscriber ID).
    /// * `sender` - The Tokio mpsc sender used for message delivery.
    pub fn new(name: impl Into<String>, sender: Sender<Arc<Message>>) -> Self {
        Self {
            name: name.into(),
            sender,
        }
    }

    /// Attempts to enqueue a message.
    ///
    /// # Behavior
    /// - Uses `try_send` to avoid blocking.
    /// - If the queue is full or disconnected, the message is dropped.
    /// - ⚠️ This follows **QoS 0**, so dropped messages are not retried.
    pub fn enqueue(&self, message: Arc<Message>) {
        if let Err(e) = self.sender.try_send(message) {
            tracing::warn!(
                queue = %self.name,
                error = %e,
                "QoS 0: Dropped message due to queue full or disconnected"
            );
        }
    }

    /// Returns the name of the queue.
    pub fn name(&self) -> &str {
        &self.name
    }
}
