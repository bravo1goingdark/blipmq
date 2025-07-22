use std::sync::Arc;
use tracing::warn;


use tracing::trace;

use crate::core::delivery_mode::DeliveryMode;
use crate::core::message::Message;
use crate::core::topics::registry::TopicRegistry;
use crate::core::topics::topic::TopicName;

/// Configuration options for the `Publisher`.
#[derive(Debug, Clone, Copy)]
pub struct PublisherConfig {
    /// Delivery mode used for all published messages.
    pub delivery_mode: DeliveryMode,
}

impl Default for PublisherConfig {
    fn default() -> Self {
        Self {
            delivery_mode: DeliveryMode::Ordered,
        }
    }
}

/// The `Publisher` is responsible for delivering messages to topics using QoS 0 (at-most-once).
///
/// # Characteristics
/// - Fire-and-forget semantics (no retries or acknowledgments).
/// - Messages are dropped if the topic does not exist.
/// - Delivery mode (ordered or unordered) is applied globally.
pub struct Publisher {
    topic_registry: Arc<TopicRegistry>,
    delivery_mode: DeliveryMode,
}

impl Publisher {
    /// Creates a new `Publisher` instance with access to the shared `TopicRegistry`
    /// and the specified `PublisherConfig`.
    #[inline]
    pub fn new(topic_registry: Arc<TopicRegistry>, config: PublisherConfig) -> Self {
        Self {
            topic_registry,
            delivery_mode: config.delivery_mode,
        }
    }

    /// Publishes a message to the given topic using the configured QoS 0 delivery mode.
    ///
    /// - If the topic is not found, the message is dropped.
    /// - No delivery confirmation is provided (QoS 0).
    pub async fn publish(&self, topic: &TopicName, message: Arc<Message>) {
        if let Some(topic_ref) = self.topic_registry.get_topic(topic) {
            topic_ref.publish_with_mode(message, self.delivery_mode).await;

            trace!(
                target: "blipmq::publisher",
                topic = ?topic,
                delivery_mode = ?self.delivery_mode,
                "Publish complete (QoS 0)"
            );
        } else {
            warn!(
                target: "blipmq::publisher",
                topic = ?topic,
                "Topic not found. Dropping message."
            );
        }
    }
}
