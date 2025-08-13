use dashmap::DashMap;
use flume::{Receiver, Sender, TryRecvError, TrySendError};
use std::sync::Arc;
use tokio::task;

use crate::core::delivery_mode::DeliveryMode;
use crate::core::error::BlipError;
use crate::core::message::{current_timestamp, Message};
use crate::core::subscriber::{Subscriber, SubscriberId};
use tracing::{debug, info, warn};

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
            // Prefetch up to this many messages per loop (reduces recv syscalls & snapshots).
            // Reuse delivery.max_batch as a sensible default knob.
            let prefetch = crate::config::CONFIG.delivery.max_batch.max(1);

            // Local buffers reused each loop to avoid allocs.
            let mut batch: Vec<Arc<Message>> = Vec::with_capacity(prefetch);
            let mut disconnected: Vec<SubscriberId> = Vec::with_capacity(8);

            loop {
                // Block for at least one message.
                let first = match rx.recv_async().await {
                    Ok(m) => m,
                    Err(_) => break, // channel closed: topic shutting down
                };
                batch.push(first);

                // Opportunistically drain more (up to `prefetch - 1`).
                while batch.len() < prefetch {
                    match rx.try_recv() {
                        Ok(m) => batch.push(m),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            // No more messages will arrive.
                            break;
                        }
                    }
                }

                // Snapshot subscribers once for the whole batch.
                let subs: Vec<Subscriber> = subscribers.iter().map(|e| e.value().clone()).collect();
                if subs.is_empty() {
                    batch.clear();
                    continue;
                }

                let now = current_timestamp();

                // Fanout each message to all subscribers.
                for msg in batch.drain(..) {
                    // Early TTL drop (saves per-sub queue work)
                    if msg.ttl_ms > 0 && now >= msg.timestamp + msg.ttl_ms {
                        continue;
                    }

                    for sub in &subs {
                        match sub.enqueue(msg.clone()) {
                            Ok(_) => {
                                debug!(target:"blipmq::topic", subscriber=%sub.id(), "enqueued");
                            }
                            Err(BlipError::Disconnected | BlipError::QueueClosed) => {
                                debug!(target:"blipmq::topic", subscriber=%sub.id(), "enqueue failed - disconnect");
                                disconnected.push(sub.id().clone());
                            }
                            Err(e) => {
                                // QoS0: ignore soft failures (e.g., full when policy is DropNew)
                                debug!(target:"blipmq::topic", subscriber=%sub.id(), ?e, "enqueue failed - ignored");
                            }
                        }
                    }
                }

                // Prune disconnected subscribers (dedupe not required; multiple removes are cheap).
                for id in disconnected.drain(..) {
                    subscribers.remove(&id);
                }
            }

            info!("Fanout task exited for topic: {}", topic_name);
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
                // Fast path: try non-blocking first to avoid await in hot path.
                match self.input_tx.try_send(message) {
                    Ok(()) => {}
                    Err(TrySendError::Full(m)) => {
                        if let Err(_) = self.input_tx.send_async(m).await {
                            warn!(
                                "Topic '{}' dropped message: input channel closed",
                                self.name
                            );
                        }
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        warn!(
                            "Topic '{}' dropped message: input channel closed",
                            self.name
                        );
                    }
                }
            }
            DeliveryMode::Parallel => {
                // Future: parallel fanout lane
                warn!(
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
