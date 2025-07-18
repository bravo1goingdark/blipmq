use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, RwLockWriteGuard};

use crate::core::message::Message;
use crate::core::queue::qos0::Queue as QoS0Queue;
use crate::core::subscriber::{Subscriber, SubscriberId};

/// Represents a topic to which subscribers can subscribe.
/// Maintains a map of subscribers, each with an individual QoS0 queue.
pub struct Topic {
    name: String,
    subscribers: RwLock<HashMap<SubscriberId, Arc<QoS0Queue>>>,
}

impl Topic {
    /// Creates a new topic with the given name.
    ///
    /// # Arguments
    /// * `name` - A string-like identifier for the topic (e.g., "chat/messages").
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            subscribers: RwLock::new(HashMap::new()),
        }
    }

    /// Returns the name of the topic.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Adds a subscriber to the topic and assigns a dedicated QoS0 queue.
    ///
    /// # Arguments
    /// * `subscriber` - The subscriber object containing id and sender.
    pub async fn subscribe(&self, subscriber: Subscriber) {
        let mut subs: RwLockWriteGuard<'_, _> = self.subscribers.write().await;
        let queue = Arc::new(QoS0Queue::new(subscriber.id.clone(), subscriber.sender));
        subs.insert(subscriber.id, queue);
    }

    /// Removes a subscriber from the topic.
    ///
    /// # Arguments
    /// * `subscriber_id` - The identifier of the subscriber to remove.
    pub async fn unsubscribe(&self, subscriber_id: &SubscriberId) {
        let mut subs = self.subscribers.write().await;
        subs.remove(subscriber_id);
    }

    /// Publishes a message to all active subscribers' queues.
    ///
    /// # Arguments
    /// * `message` - An Arc-wrapped message to deliver.
    pub fn publish(&self, message: Arc<Message>) {
        let subscribers = self.subscribers.blocking_read();
        for queue in subscribers.values() {
            queue.enqueue(message.clone());
        }
    }
}
