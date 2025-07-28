use dashmap::DashMap;
use std::sync::Arc;
use tokio::task;

use flume::{Receiver, Sender};

use crate::core::delivery_mode::DeliveryMode;
use crate::core::error::BlipError;
use crate::core::message::Message;
use crate::core::queue::qos0::Queue as QoS0Queue;
use crate::core::queue::QueueBehavior;
use crate::core::subscriber::{Subscriber, SubscriberId};

pub type TopicName = String;

/// A Topic holds a list of subscribers and a fanout task.
///
/// Messages are first sent to a bounded flume::Sender,
/// and then fanned out to per-subscriber queues in a dedicated task.
#[derive(Debug)]
pub struct Topic {
    name: TopicName,
    subscribers: DashMap<SubscriberId, Arc<QoS0Queue>>,
    input_tx: Sender<Arc<Message>>,
}

impl Topic {
    /// Creates a new topic with a bounded flume queue and spawns the fanout task.
    pub fn new(name: impl Into<TopicName>, queue_capacity: usize) -> Self {
        let name = name.into();
        let (tx, rx) = flume::bounded(queue_capacity);
        let topic = Self {
            name: name.clone(),
            subscribers: DashMap::new(),
            input_tx: tx,
        };
        topic.spawn_fanout_task(rx);
        topic
    }

    /// Starts the fanout loop: pulls from flume and sends to subscriber queues.
    fn spawn_fanout_task(&self, rx: Receiver<Arc<Message>>) {
        let subscribers = self.subscribers.clone();
        let topic_name = self.name.clone();

        task::spawn(async move {
            while let Ok(message) = rx.recv_async().await {
                let mut disconnected = vec![];

                for entry in subscribers.iter() {
                    let queue = entry.value();

                    if let Err(err) = queue.enqueue(message.clone()) {
                        match err {
                            BlipError::Disconnected | BlipError::QueueClosed => {
                                disconnected.push(entry.key().clone());
                            }
                            _ => {} // other errors: ignore
                        }
                    }
                }

                for id in disconnected {
                    subscribers.remove(&id);
                }
            }

            tracing::info!("Fanout task exited for topic: {}", topic_name);
        });
    }

    /// Sends a message into the topic's input queue.
    pub async fn publish(&self, message: Arc<Message>) {
        self.publish_with_mode(message, DeliveryMode::Ordered).await;
    }

    /// Bounded enqueue to topic-level flume channel (reactive backpressure).
    pub async fn publish_with_mode(&self, message: Arc<Message>, _mode: DeliveryMode) {
        // Currently only Ordered is supported
        if let Err(_) = self.input_tx.send_async(message).await {
            tracing::warn!(
                "Topic '{}' dropped message: input channel closed",
                self.name
            );
        }
    }

    /// Registers a subscriber with a bounded QoS0 queue.
    ///
    /// # Arguments
    ///
    /// * `subscriber` – the new subscriber (owns its ID and TCP sender)
    /// * `capacity` – max number of messages this subscriber can buffer
    pub async fn subscribe(&self, subscriber: Subscriber, capacity: usize) {
        // Clone the SubscriberId so we pass an owned String into QoS0Queue::new
        let subscriber_id = subscriber.id().clone();

        // Create the per-subscriber SPSC queue
        let queue = Arc::new(QoS0Queue::new(subscriber_id.clone(), capacity));

        // Insert into our DashMap keyed by owned SubscriberId
        self.subscribers.insert(subscriber_id, queue);
    }

    /// Removes a subscriber from this topic.
    pub async fn unsubscribe(&self, subscriber_id: &SubscriberId) {
        self.subscribers.remove(subscriber_id);
    }

    /// Returns topic name.
    pub fn name(&self) -> &TopicName {
        &self.name
    }
}
