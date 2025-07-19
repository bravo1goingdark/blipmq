use dashmap::DashMap;
use std::sync::Arc;
use tokio::task;

use crate::core::message::Message;
use crate::core::queue::qos0::Queue as QoS0Queue;
use crate::core::queue::QueueBehavior;
use crate::core::subscriber::{Subscriber, SubscriberId};
use crate::core::delivery_mode::DeliveryMode;

/// Alias for a topic name.
pub type TopicName = String;

/// A topic represents a named channel for message distribution.
/// It handles multiple subscribers and delivers messages to all of them.
#[derive(Debug)]
pub struct Topic {
    name: TopicName,

    /// Concurrent map of subscriber ID to their queue.
    subscribers: DashMap<SubscriberId, Arc<dyn QueueBehavior + Send + Sync>>,
}

impl Topic {
    /// Creates a new topic with a name.
    pub fn new(name: impl Into<TopicName>) -> Self {
        Self {
            name: name.into(),
            subscribers: DashMap::new(),
        }
    }

    /// Returns the topic's name.
    pub fn name(&self) -> &TopicName {
        &self.name
    }

    /// Subscribes a new subscriber to this topic.
    ///
    /// If the subscriber is disconnected (`sender == None`), it is ignored.
    pub async fn subscribe(&self, subscriber: Subscriber) {
        if let Some(sender) = subscriber.sender() {
            let queue: Arc<dyn QueueBehavior + Send + Sync> = Arc::new(QoS0Queue::new(
                subscriber.id().clone(),
                sender.clone(),
            ));
            self.subscribers.insert(subscriber.id().clone(), queue);
        } else {
            tracing::warn!(
                target: "blipmq::topic",
                subscriber_id = %subscriber.id(),
                "Skipping subscription: subscriber is disconnected"
            );
        }
    }

    /// Removes a subscriber by ID.
    pub async fn unsubscribe(&self, subscriber_id: &SubscriberId) {
        self.subscribers.remove(subscriber_id);
    }

    /// Publishes a message to all subscribers with ordered delivery (default).
    pub async fn publish(&self, message: Arc<Message>) {
        self.publish_with_mode(message, DeliveryMode::Ordered).await;
    }

    /// Publishes a message to all subscribers using the specified delivery mode.
    ///
    /// - Ordered: enqueues sequentially (preserving order)
    /// - Unordered: enqueues in parallel (order not guaranteed)
    pub async fn publish_with_mode(&self, message: Arc<Message>, mode: DeliveryMode) {
        match mode {
            DeliveryMode::Ordered => {
                for queue in self.subscribers.iter() {
                    queue.value().enqueue(message.clone());
                }
            }
            DeliveryMode::Unordered => {
                let mut handles = Vec::with_capacity(self.subscribers.len());

                for queue in self.subscribers.iter() {
                    let msg_clone = message.clone();
                    let queue_clone = queue.value().clone();

                    let handle = task::spawn(async move {
                        queue_clone.enqueue(msg_clone);
                    });

                    handles.push(handle);
                }

                for handle in handles {
                    let _ = handle.await;
                }
            }
        }
    }
}
