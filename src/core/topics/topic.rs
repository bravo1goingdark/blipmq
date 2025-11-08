use ahash::AHasher;
use dashmap::DashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::task;
use tracing::debug;

use crate::config::CONFIG;
use crate::core::delivery_mode::DeliveryMode;
use crate::core::error::BlipError;
use crate::core::lockfree::MpmcQueue;
use crate::core::message::{to_wire_message, Message, WireMessage};
use crate::core::subscriber::{Subscriber, SubscriberId};
use crate::util::backoff::AdaptiveYield;

pub type TopicName = String;

/// A Topic holds a list of subscribers and orchestrates a high-performance fanout pipeline.
pub struct Topic {
    name: TopicName,
    subscribers: Arc<DashMap<SubscriberId, Subscriber>>, // global view
    shards: Vec<Shard>,
    ingress_queue: Arc<MpmcQueue<Arc<Message>>>,
    ingress_notify: Arc<Notify>,
}

struct Shard {
    subs: Arc<DashMap<SubscriberId, Subscriber>>, // partitioned subscribers
    queue: Arc<MpmcQueue<Arc<WireMessage>>>,
    notify: Arc<Notify>,
}

impl Topic {
    pub fn new(name: impl Into<TopicName>, queue_capacity: usize) -> Self {
        let name = name.into();

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

        let mut shards: Vec<Shard> = Vec::with_capacity(shard_count);
        let global_subs: Arc<DashMap<SubscriberId, Subscriber>> = Arc::new(DashMap::new());
        let shard_queue_capacity = queue_capacity.max(1024);
        for shard_index in 0..shard_count {
            let shard_subs: Arc<DashMap<SubscriberId, Subscriber>> = Arc::new(DashMap::new());
            let shard_queue = Arc::new(MpmcQueue::new(shard_queue_capacity));
            let shard_notify = Arc::new(Notify::new());

            Self::spawn_shard_worker(
                shard_index,
                Arc::clone(&shard_subs),
                Arc::clone(&global_subs),
                Arc::clone(&shard_queue),
                Arc::clone(&shard_notify),
            );

            shards.push(Shard {
                subs: shard_subs,
                queue: shard_queue,
                notify: shard_notify,
            });
        }

        let ingress_queue = Arc::new(MpmcQueue::new(queue_capacity.max(1024)));
        let ingress_notify = Arc::new(Notify::new());

        let topic = Self {
            name: name.clone(),
            subscribers: global_subs,
            shards,
            ingress_queue,
            ingress_notify,
        };

        topic.spawn_dispatcher();
        topic
    }

    fn spawn_dispatcher(&self) {
        let ingress_queue = Arc::clone(&self.ingress_queue);
        let ingress_notify = Arc::clone(&self.ingress_notify);
        let shard_channels: Vec<(Arc<MpmcQueue<Arc<WireMessage>>>, Arc<Notify>)> = self
            .shards
            .iter()
            .map(|s| (Arc::clone(&s.queue), Arc::clone(&s.notify)))
            .collect();

        task::spawn(async move {
            loop {
                while let Some(message) = ingress_queue.try_dequeue() {
                    let wire = Arc::new(to_wire_message(&message));
                    for (queue, notify) in &shard_channels {
                        push_with_backpressure(queue, notify, wire.clone()).await;
                    }
                }

                ingress_notify.notified().await;
            }
        });
    }

    fn spawn_shard_worker(
        shard_index: usize,
        shard_subs: Arc<DashMap<SubscriberId, Subscriber>>,
        global_subs: Arc<DashMap<SubscriberId, Subscriber>>,
        queue: Arc<MpmcQueue<Arc<WireMessage>>>,
        notify: Arc<Notify>,
    ) {
        task::spawn(async move {
            crate::util::affinity::set_current_thread_affinity(shard_index);
            let mut disconnected: Vec<SubscriberId> = Vec::with_capacity(16);
            loop {
                while let Some(wire) = queue.try_dequeue() {
                    let now = crate::core::message::current_timestamp();
                    if wire.is_expired(now) {
                        crate::metrics::inc_dropped_ttl(1);
                        continue;
                    }

                    let mut subs_snapshot: Vec<Subscriber> = Vec::with_capacity(shard_subs.len());
                    for e in shard_subs.iter() {
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
                            Err(_) => {}
                        }
                    }

                    for id in disconnected.drain(..) {
                        if let Some(_) = shard_subs.remove(&id) {
                            global_subs.remove(&id);
                            debug!("Removed disconnected subscriber: {}", id);
                        }
                    }
                }

                notify.notified().await;
            }
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
                push_with_backpressure(&self.ingress_queue, &self.ingress_notify, message).await;
            }
            DeliveryMode::Parallel => {
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
        self.subscribers
            .insert(subscriber_id.clone(), subscriber.clone());
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

impl fmt::Debug for Topic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Topic")
            .field("name", &self.name)
            .field("subscribers", &self.subscribers.len())
            .field("shards", &self.shards.len())
            .finish()
    }
}

impl fmt::Debug for Shard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shard")
            .field("subscribers", &self.subs.len())
            .finish()
    }
}

async fn push_with_backpressure<T>(queue: &Arc<MpmcQueue<T>>, notify: &Arc<Notify>, mut item: T)
where
    T: Send,
{
    let mut backoff = AdaptiveYield::new();
    loop {
        match queue.try_enqueue(item) {
            Ok(_) => {
                notify.notify_one();
                break;
            }
            Err(returned) => {
                item = returned;
                backoff.snooze().await;
            }
        }
    }
}
