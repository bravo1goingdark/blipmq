use std::sync::Arc;
use tracing::{debug, warn};

use crate::core::message::Message;
use crate::core::topics::topic::{TopicName, TopicRegistry};

/// Responsible for publishing messages to topics using QoS 0 delivery.
///
/// - If the topic does not exist, the message is dropped.
/// - If delivery fails, it is also silently ignored (as per QoS 0).
pub struct Publisher {
    topic_registry: Arc<TopicRegistry>,
}

impl Publisher {
    /// Creates a new publisher with access to the shared topic registry.
    ///
    /// # Arguments
    /// * `topic_registry` - A thread-safe reference to the topic registry.
    pub fn new(topic_registry: Arc<TopicRegistry>) -> Self {
        Self { topic_registry }
    }

    /// Publishes a message to the specified topic.
    ///
    /// # Arguments
    /// * `topic` - The name of the topic to publish to.
    /// * `message` - The message to be published.
    ///
    /// # QoS
    /// QoS 0: fire-and-forget. Failures are silently ignored.
    pub fn publish(&self, topic: &TopicName, message: Arc<Message>) {
        match self.topic_registry.get_topic(topic) {
            Some(topic_ref) => {
                debug!(target: "publisher", topic = ?topic, "Publishing message");
                topic_ref.publish(message);
            }
            None => {
                // QoS 0 : silently ignore
                warn!(target: "publisher", topic = ?topic, "Topic not found. Message dropped.");
            }
        }
    }
}
