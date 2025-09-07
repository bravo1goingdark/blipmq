use dashmap::DashMap;
use flume::{Receiver, Sender};
use std::sync::Arc;
use tokio::task;

use crate::config::CONFIG;
use crate::core::delivery_mode::DeliveryMode;
use crate::core::error::BlipError;
use crate::core::message::{to_wire_message, Message, WireMessage};
use crate::core::subscriber::{Subscriber, SubscriberId};
use ahash::AHasher;
use std::hash::{Hash, Hasher};
use tokio::sync::mpsc;

pub type TopicName = String;

/// A Topic holds a list of subscribers and a fanout task.
///
/// Messages are first sent to a bounded flume::Sender,
/// and then fanned out to per-subscriber queues in a dedicated task.
#[derive(Debug)]
pub struct Topic {
    name: TopicName,
    subscribers: Arc<DashMap<SubscriberId, Subscriber>>, // global view
    shards: Vec<Shard>,
    input_tx: Sender<Arc<Message>>,
}

#[derive(Debug)]
struct Shard {
    subs: Arc<DashMap<SubscriberId, Subscriber>>, // partitioned subscribers
    tx: mpsc::Sender<Arc<WireMessage>>,           // wire frames to process
}

impl Topic {
    pub fn new(name: impl Into<TopicName>, queue_capacity: usize) -> Self {
        let name = name.into();
        let (tx, rx) = flume::bounded(queue_capacity);

        // Determine shard count based on config or available parallelism
        let shard_count = {
            let cfg = CONFIG.delivery.fanout_shards;
            if cfg > 0 {
                cfg
            } else {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(2)
            }
        }
        .clamp(1, 16);

        // Create shards and their worker channels
        let mut shards: Vec<Shard> = Vec::with_capacity(shard_count);
        let global_subs: Arc<DashMap<SubscriberId, Subscriber>> = Arc::new(DashMap::new());
        let topic_name_for_workers = name.clone();
        for shard_index in 0..shard_count {
            let (s_tx, mut s_rx) = mpsc::channel::<Arc<WireMessage>>(1024);
            let shard_subs: Arc<DashMap<SubscriberId, Subscriber>> = Arc::new(DashMap::new());
            let shard_subs_for_task = Arc::clone(&shard_subs);
            let global_subs_clone = Arc::clone(&global_subs);
            let topic_name_clone = topic_name_for_workers.clone();

            // Worker task for this shard
            task::spawn(async move {
                // Best-effort: attempt to pin this worker to a specific core (no-op on non-Windows)
                crate::util::affinity::set_current_thread_affinity(shard_index);
                // Reuse buffers locally in the worker as needed
                let mut disconnected: Vec<SubscriberId> = Vec::with_capacity(16);
                loop {
                    match s_rx.recv().await {
                        Some(wire) => {
                            // TTL drop early (keeps queues small)
                            let now = crate::core::message::current_timestamp();
                            if wire.is_expired(now) {
                                crate::metrics::inc_dropped_ttl(1);
                                continue;
                            }

                            // Snapshot this shard's subscribers (pre-size to reduce reallocations)
                            let mut subs_snapshot: Vec<Subscriber> =
                                Vec::with_capacity(shard_subs_for_task.len());
                            for e in shard_subs_for_task.iter() {
                                subs_snapshot.push(e.value().clone());
                            }

                            disconnected.clear();
                            for subscriber in subs_snapshot.iter() {
                                let id = subscriber.id().clone();
                                let res = subscriber.enqueue(wire.clone());
                                match res {
                                    Ok(_) => {
                                        crate::metrics::inc_enqueued(1);
                                    }
                                    Err(BlipError::Disconnected | BlipError::QueueClosed) => {
                                        disconnected.push(id);
                                    }
                                    Err(BlipError::QueueFull) => {
                                        crate::metrics::inc_dropped_sub_queue_full(1);
                                    }
                                    Err(_) => {
                                        // ignore others
                                    }
                                }
                            }

                            // Remove disconnected from shard and global registries
                            for id in disconnected.drain(..) {
                                shard_subs_for_task.remove(&id);
                                global_subs_clone.remove(&id);
                            }
                        }
                        None => {
                            tracing::info!("Shard worker exited for topic: {}", topic_name_clone);
                            break;
                        }
                    }
                }
            });

            shards.push(Shard {
                subs: shard_subs,
                tx: s_tx,
            });
        }

        let topic = Self {
            name: name.clone(),
            subscribers: global_subs,
            shards,
            input_tx: tx,
        };

        topic.spawn_fanout_task(rx);
        topic
    }

    fn spawn_fanout_task(&self, rx: Receiver<Arc<Message>>) {
        let shard_senders: Vec<mpsc::Sender<Arc<WireMessage>>> =
            self.shards.iter().map(|s| s.tx.clone()).collect();
        let topic_name = self.name.clone();

        task::spawn(async move {
            while let Ok(message) = rx.recv_async().await {
                // Pre-encode the wire frame once for this published message
                let wire = Arc::new(to_wire_message(&message));
                // Send to all shard workers
                for tx in &shard_senders {
                    if tx.try_send(wire.clone()).is_err() {
                        // fall back to await if channel is full
                        if tx.send(wire.clone()).await.is_err() {
                            // shard worker exited; continue
                            continue;
                        }
                    }
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
                if (self.input_tx.send_async(message).await).is_err() {
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
    pub fn subscribe(&self, subscriber: Subscriber, _capacity: usize) {
        let subscriber_id = subscriber.id().clone();
        // Insert into global registry
        self.subscribers
            .insert(subscriber_id.clone(), subscriber.clone());
        // Insert into shard
        let idx = self.shard_index(&subscriber_id);
        self.shards[idx].subs.insert(subscriber_id, subscriber);
    }

    /// Removes a subscriber from this topic.
    pub fn unsubscribe(&self, subscriber_id: &SubscriberId) {
        self.subscribers.remove(subscriber_id);
        let idx = self.shard_index(subscriber_id);
        self.shards[idx].subs.remove(subscriber_id);
    }

    pub fn name(&self) -> &TopicName {
        &self.name
    }

    #[inline]
    fn shard_index(&self, id: &SubscriberId) -> usize {
        let mut hasher = AHasher::default();
        id.hash(&mut hasher);
        let h = hasher.finish() as usize;
        let n = self.shards.len().max(1);
        h % n
    }
}
