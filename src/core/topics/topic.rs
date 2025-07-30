use dashmap::DashMap;
use flume::{Receiver, Sender};
use std::sync::Arc;
use tokio::task;

use crate::core::delivery_mode::DeliveryMode;
use crate::core::error::BlipError;
use crate::core::message::Message;
use crate::core::subscriber::{Subscriber, SubscriberId};
use futures::stream::{FuturesUnordered, StreamExt};

pub type TopicName = String;

/// A Topic holds a list of subscribers and a fanout task.
///
/// Messages are first sent to a bounded flume::Sender,
/// and then fanned out to per-subscriber queues in a dedicated task.
#[derive(Debug)]
pub struct Topic {
    name: TopicName,
    subscribers: Arc<DashMap<SubscriberId, Subscriber>>,
    input_tx: Sender<Arc<Message>>,
}

impl Topic {
    pub fn new(name: impl Into<TopicName>, queue_capacity: usize) -> Self {
        let name = name.into();
        let (tx, rx) = flume::bounded(queue_capacity);
        let topic = Self {
            name: name.clone(),
            subscribers: Arc::new(DashMap::new()),
            input_tx: tx,
        };
        topic.spawn_fanout_task(rx);
        topic
    }

    fn spawn_fanout_task(&self, rx: Receiver<Arc<Message>>) {
        let subscribers = Arc::clone(&self.subscribers);
        let topic_name = self.name.clone();

        task::spawn(async move {
            while let Ok(message) = rx.recv_async().await {
                // Snapshot subscribers (avoid DashMap lock during loop)
                let subs: Vec<Subscriber> = subscribers
                    .iter()
                    .map(|entry| entry.value().clone())
                    .collect();

                // Concurrently attempt to enqueue message to each subscriber
                let mut fanout = FuturesUnordered::new();
                for subscriber in subs {
                    let msg = message.clone();
                    fanout.push(async move {
                        let id = subscriber.id().clone();
                        let res = subscriber.enqueue(msg);
                        (id, res)
                    });
                }

                // Collect disconnected subscribers
                let mut disconnected = Vec::new();
                while let Some((id, res)) = fanout.next().await {
                    match res {
                        Ok(_) => {
                            tracing::debug!(target="blipmq::topic", subscriber=%id, "enqueued");
                        }
                        Err(BlipError::Disconnected | BlipError::QueueClosed) => {
                            tracing::debug!(target="blipmq::topic", subscriber=%id, ?res, "enqueue failed - disconnect");
                            disconnected.push(id);
                        }
                        Err(e) => {
                            tracing::debug!(target="blipmq::topic", subscriber=%id, ?e, "enqueue failed - ignored");
                        }
                    }
                }

                // Remove disconnected subscribers from registry
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

    /// Push message to topic's queue (backpressure-aware).
    pub async fn publish_with_mode(&self, message: Arc<Message>, mode: DeliveryMode) {
        match mode {
            DeliveryMode::Ordered => {
                if let Err(_) = self.input_tx.send_async(message).await {
                    tracing::warn!(
                        "Topic '{}' dropped message: input channel closed",
                        self.name
                    );
                }
            }
            DeliveryMode::Parallel => {
                // Future: support alternate path for parallel fanout logic
                tracing::warn!(
                    "Parallel delivery mode not yet supported for topic '{}'",
                    self.name
                );
            }
        }
    }

    /// Registers a subscriber.
    pub async fn subscribe(&self, subscriber: Subscriber, _capacity: usize) {
        let subscriber_id = subscriber.id().clone();
        self.subscribers.insert(subscriber_id, subscriber);
    }

    /// Removes a subscriber from this topic.
    pub async fn unsubscribe(&self, subscriber_id: &SubscriberId) {
        self.subscribers.remove(subscriber_id);
    }

    pub fn name(&self) -> &TopicName {
        &self.name
    }
}
