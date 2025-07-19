use std::sync::Arc;
use crossbeam_queue::SegQueue;
use tokio::sync::mpsc::UnboundedSender;
use tracing::warn;

use crate::core::message::Message;
use crate::core::queue::QueueBehavior;

/// A zero-copy, ultra-lightweight queue for QoS 0 (fire-and-forget) delivery.
///
///  Lock-free via `SegQueue`
///
///  Direct push into subscriber channel
///
///  Drops message if send fails (no retries)
///
///  Internal buffer for debug/dequeue inspection (not used for real delivery)
#[derive(Debug)]
pub struct Queue {
    name: String,
    sender: UnboundedSender<Arc<Message>>,
    buffer: Arc<SegQueue<Arc<Message>>>, // optional debug/dequeue tracking
}

impl Queue {
    /// Creates a new QoS 0 queue instance.
    ///
    /// # Arguments
    /// - `name`: Queue name (typically the SubscriberId)
    /// - `sender`: Channel to deliver messages to the subscriber
    pub fn new(name: impl Into<String>, sender: UnboundedSender<Arc<Message>>) -> Self {
        Self {
            name: name.into(),
            sender,
            buffer: Arc::new(SegQueue::new()),
        }
    }
}

impl QueueBehavior for Queue {
    /// Immediately attempts to send the message.
    ///
    /// If the receiver has dropped, the message is discarded.
    /// Also pushes into the debug buffer (optional, non-critical).
    fn enqueue(&self, message: Arc<Message>) {
        if let Err(e) = self.sender.send(message.clone()) {
            warn!(
                target: "blipmq::queue::qos0",
                queue = %self.name,
                error = %e,
                "QoS 0: Dropped message — receiver unavailable"
            );
        }

        // Debug / metrics use only — does not affect delivery
        self.buffer.push(message);
    }

    /// Debug-only: pops a message from internal buffer.
    ///
    /// Has no impact on actual delivery path.
    fn dequeue(&self) -> Option<Arc<Message>> {
        self.buffer.pop()
    }

    /// Returns approximate size of the debug buffer.
    fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Returns queue name (typically subscriber ID).
    fn name(&self) -> &str {
        &self.name
    }
}
