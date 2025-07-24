use dashmap::DashMap;
use std::sync::Arc;

use crate::core::delivery_mode::DeliveryMode;
use crate::core::message::Message;
use crate::core::queue::qos0::Queue as QoS0Queue;
use crate::core::queue::QueueBehavior;
use crate::core::subscriber::{Subscriber, SubscriberId};

pub type TopicName = String;

#[derive(Debug)]
pub struct Topic {
    name: TopicName,
    subscribers: DashMap<SubscriberId, Arc<dyn QueueBehavior + Send + Sync>>,
}

impl Topic {
    pub fn new(name: impl Into<TopicName>) -> Self {
        let subscribers = DashMap::new();

        Self {
            name: name.into(),
            subscribers,
        }
    }

    pub fn name(&self) -> &TopicName {
        &self.name
    }

    pub async fn subscribe(&self, subscriber: Subscriber) {
        let queue: Arc<dyn QueueBehavior + Send + Sync> = Arc::new(QoS0Queue::new(
            subscriber.id().clone(),
            subscriber.sender().clone(),
        ));
        self.subscribers.insert(subscriber.id().clone(), queue);
    }

    pub async fn unsubscribe(&self, subscriber_id: &SubscriberId) {
        self.subscribers.remove(subscriber_id);
    }

    pub async fn publish(&self, message: Arc<Message>) {
        self.publish_with_mode(message, DeliveryMode::Ordered).await;
    }

    pub async fn publish_with_mode(&self, message: Arc<Message>, mode: DeliveryMode) {
        match mode {
            DeliveryMode::Ordered => {
                let mut disconnected = vec![];

                for entry in self.subscribers.iter() {
                    if entry.value().enqueue(message.clone()).is_err() {
                        disconnected.push(entry.key().clone());
                    }
                }

                for id in disconnected {
                    self.subscribers.remove(&id);
                }
            }
        }
    }
}
