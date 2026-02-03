use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use parking_lot::{Mutex, RwLock};
use wal::{WalError as LogError, WriteAheadLog};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QoSLevel {
    AtMostOnce,
    AtLeastOnce,
}

#[derive(Debug, Clone)]
pub struct BrokerConfig {
    pub default_qos: QoSLevel,
    pub message_ttl: Duration,
    /// Default per-subscriber in-memory queue capacity in number of messages.
    pub per_subscriber_queue_capacity: usize,
    /// Maximum number of delivery attempts for QoS1 messages before dropping.
    pub max_retries: u32,
    /// Base delay used for exponential backoff between retry attempts.
    pub retry_base_delay: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TopicName(String);

impl TopicName {
    pub fn new<S: Into<String>>(name: S) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientId(String);

impl ClientId {
    pub fn new<S: Into<String>>(id: S) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

impl SubscriptionId {
    pub fn value(self) -> u64 {
        self.0
    }

    pub fn from_raw(value: u64) -> Self {
        SubscriptionId(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeliveryTag(u64);

impl DeliveryTag {
    pub fn value(self) -> u64 {
        self.0
    }

    pub fn from_raw(value: u64) -> Self {
        DeliveryTag(value)
    }
}

#[derive(Debug, Clone)]
pub struct PolledMessage {
    pub subscription_id: SubscriptionId,
    pub topic: TopicName,
    pub payload: Bytes,
    pub qos: QoSLevel,
    /// Present only for QoS1 deliveries that must be acked.
    pub delivery_tag: Option<DeliveryTag>,
}

#[derive(Debug)]
pub struct Broker {
    config: BrokerConfig,
    topics: TopicShards,
    subscriptions: RwLock<HashMap<SubscriptionId, SubscriptionRef>>,
    next_subscription_id: AtomicU64,
    wal: Option<Arc<WriteAheadLog>>,
    shutting_down: std::sync::atomic::AtomicBool,
    messages_published_total: std::sync::atomic::AtomicU64,
    messages_delivered_total: std::sync::atomic::AtomicU64,
}

#[derive(Debug)]
struct Topic {
    #[allow(dead_code)]
    name: TopicName,
    subscribers: RwLock<HashMap<SubscriptionId, Arc<Subscriber>>>,
}

#[derive(Debug)]
struct TopicShards {
    shards: Vec<RwLock<HashMap<TopicName, Arc<Topic>>>>,
}

impl TopicShards {
    fn new(num_shards: usize) -> Self {
        let mut shards = Vec::with_capacity(num_shards);
        for _ in 0..num_shards {
            shards.push(RwLock::new(HashMap::new()));
        }
        Self { shards }
    }

    fn shard_index(topic: &TopicName, len: usize) -> usize {
        use std::collections::hash_map::DefaultHasher;
        let mut hasher = DefaultHasher::new();
        topic.hash(&mut hasher);
        (hasher.finish() as usize) % len
    }

    fn get_or_insert(&self, topic: TopicName) -> Arc<Topic> {
        let len = self.shards.len();
        let idx = Self::shard_index(&topic, len);
        {
            let guard = self.shards[idx].read();
            if let Some(t) = guard.get(&topic) {
                return t.clone();
            }
        }
        // Upgrade to write lock if not present.
        let mut guard = self.shards[idx].write();
        guard
            .entry(topic.clone())
            .or_insert_with(|| Arc::new(Topic::new(topic)))
            .clone()
    }

    fn get(&self, topic: &TopicName) -> Option<Arc<Topic>> {
        let len = self.shards.len();
        let idx = Self::shard_index(topic, len);
        let guard = self.shards[idx].read();
        guard.get(topic).cloned()
    }

    fn len(&self) -> usize {
        self.shards.iter().map(|s| s.read().len()).sum()
    }
}

#[derive(Debug)]
struct Subscriber {
    #[allow(dead_code)]
    client_id: ClientId,
    queue: SubscriberQueue,
}

#[derive(Debug)]
struct SubscriptionRef {
    topic: TopicName,
    subscriber: Arc<Subscriber>,
}

#[derive(Debug)]
struct SubscriberQueue {
    default_qos: QoSLevel,
    capacity: usize,
    inner: Mutex<SubscriberQueueInner>,
}

#[derive(Debug)]
struct SubscriberQueueInner {
    next_tag: u64,
    pending: VecDeque<QueueEntry>,
    inflight: HashMap<DeliveryTag, QueueEntry>,
}

#[derive(Debug, Clone)]
struct QueueEntry {
    tag: DeliveryTag,
    payload: Bytes,
    qos: QoSLevel,
    #[allow(dead_code)]
    wal_id: Option<u64>,
    created_at: std::time::Instant,
    ttl: Option<Duration>,
    delivery_attempts: u32,
    next_delivery_at: std::time::Instant,
}

#[derive(Debug, Clone)]
struct WalMessageRecord {
    topic: String,
    qos: QoSLevel,
    payload: Bytes,
}

impl WalMessageRecord {
    fn encode(&self) -> Result<Vec<u8>, LogError> {
        let mut buf = BytesMut::new();

        let qos_byte = match self.qos {
            QoSLevel::AtMostOnce => 0u8,
            QoSLevel::AtLeastOnce => 1u8,
        };
        buf.put_u8(qos_byte);

        let topic_bytes = self.topic.as_bytes();
        let topic_len = topic_bytes.len();
        let topic_len_u16 = u16::try_from(topic_len)
            .map_err(|_| LogError::Corruption("topic name too long for WAL record".to_string()))?;
        buf.put_u16(topic_len_u16);
        buf.put_slice(topic_bytes);

        buf.put_slice(&self.payload);

        Ok(buf.to_vec())
    }

    fn decode(bytes: &[u8]) -> Result<Self, LogError> {
        if bytes.len() < 3 {
            return Err(LogError::Corruption(
                "WAL record too short to contain header".to_string(),
            ));
        }

        let mut slice = bytes;
        let qos_byte = slice.get_u8();
        let qos = match qos_byte {
            0 => QoSLevel::AtMostOnce,
            1 => QoSLevel::AtLeastOnce,
            _ => {
                return Err(LogError::Corruption(format!(
                    "invalid QoS value {qos_byte} in WAL record"
                )))
            }
        };

        let topic_len = slice.get_u16() as usize;
        if slice.remaining() < topic_len {
            return Err(LogError::Corruption(
                "WAL record truncated while reading topic".to_string(),
            ));
        }

        let topic_bytes = slice.copy_to_bytes(topic_len);
        let topic = String::from_utf8(topic_bytes.to_vec()).map_err(|_| {
            LogError::Corruption("topic name in WAL record is not valid UTF-8".to_string())
        })?;

        let payload = slice.copy_to_bytes(slice.remaining());

        Ok(Self {
            topic,
            qos,
            payload,
        })
    }
}

impl SubscriberQueue {
    fn new(default_qos: QoSLevel, capacity: usize) -> Self {
        Self {
            default_qos,
            capacity,
            inner: Mutex::new(SubscriberQueueInner {
                next_tag: 1,
                pending: VecDeque::new(),
                inflight: HashMap::new(),
            }),
        }
    }

    #[inline(always)]
    fn enqueue(
        &self,
        payload: Bytes,
        message_qos: QoSLevel,
        wal_id: Option<u64>,
        ttl: Option<Duration>,
    ) {
        let mut inner = self.inner.lock();

        // Bound total number of stored messages.
        while inner.pending.len() + inner.inflight.len() >= self.capacity {
            inner.pending.pop_front();
            if inner.pending.is_empty() {
                break;
            }
        }

        let tag = DeliveryTag(inner.next_tag);
        inner.next_tag = inner.next_tag.wrapping_add(1);

        let effective_qos = match (self.default_qos, message_qos) {
            (QoSLevel::AtLeastOnce, QoSLevel::AtLeastOnce) => QoSLevel::AtLeastOnce,
            _ => QoSLevel::AtMostOnce,
        };

        let now = std::time::Instant::now();

        inner.pending.push_back(QueueEntry {
            tag,
            payload,
            qos: effective_qos,
            wal_id,
            created_at: now,
            ttl,
            delivery_attempts: 0,
            next_delivery_at: now,
        });
    }

    #[inline(always)]
    fn dequeue(&self, base_delay: Duration) -> Option<(QueueEntry, Option<DeliveryTag>)> {
        let mut inner = self.inner.lock();
        let mut entry = inner.pending.pop_front()?;

        let span = tracing::trace_span!("subscriber_dequeue", qos = ?entry.qos);
        let _guard = span.enter();

        match entry.qos {
            QoSLevel::AtMostOnce => {
                // Fire-and-forget: do not track in inflight.
                let tag = None;
                Some((entry, tag))
            }
            QoSLevel::AtLeastOnce => {
                entry.delivery_attempts = entry.delivery_attempts.saturating_add(1);
                let shift = entry.delivery_attempts.saturating_sub(1).min(31);
                let mut factor: u32 = 1;
                for _ in 0..shift {
                    factor = factor.saturating_mul(2);
                }
                let backoff = base_delay.checked_mul(factor).unwrap_or(base_delay);
                entry.next_delivery_at = std::time::Instant::now() + backoff;

                let tag = Some(entry.tag);
                inner.inflight.insert(entry.tag, entry.clone());
                Some((entry, tag))
            }
        }
    }

    #[allow(dead_code)]
    fn peek(&self) -> Option<Bytes> {
        let inner = self.inner.lock();
        inner.pending.front().map(|e| e.payload.clone())
    }

    fn ack(&self, tag: DeliveryTag) -> bool {
        let mut inner = self.inner.lock();
        inner.inflight.remove(&tag).is_some()
    }

    fn inflight_len(&self) -> usize {
        let inner = self.inner.lock();
        inner.inflight.len()
    }

    fn maintenance_tick(&self, now: std::time::Instant, max_retries: u32, _base_delay: Duration) {
        let mut inner = self.inner.lock();

        // Drop expired messages from pending.
        inner.pending.retain(|entry| {
            if let Some(ttl) = entry.ttl {
                entry.created_at + ttl > now
            } else {
                true
            }
        });

        // Drop expired messages from inflight.
        inner.inflight.retain(|_, entry| {
            if let Some(ttl) = entry.ttl {
                entry.created_at + ttl > now
            } else {
                true
            }
        });

        // Collect inflight messages that should be retried or dropped.
        let mut to_retry = Vec::new();
        let mut to_drop = Vec::new();

        for (tag, entry) in inner.inflight.iter() {
            if now >= entry.next_delivery_at {
                if entry.delivery_attempts < max_retries {
                    to_retry.push(*tag);
                } else {
                    to_drop.push(*tag);
                }
            }
        }

        for tag in to_drop {
            inner.inflight.remove(&tag);
        }

        for tag in to_retry {
            if let Some(mut entry) = inner.inflight.remove(&tag) {
                // Reset next_delivery_at; it will be set when the message is
                // delivered again via `dequeue`.
                entry.next_delivery_at = now;
                inner.pending.push_back(entry);
            }
        }
    }
}

impl Topic {
    fn new(name: TopicName) -> Self {
        Self {
            name,
            subscribers: RwLock::new(HashMap::new()),
        }
    }
}

impl Broker {
    #[inline(always)]
    pub fn new(config: BrokerConfig) -> Self {
        Self {
            topics: TopicShards::new(16),
            subscriptions: RwLock::new(HashMap::new()),
            next_subscription_id: AtomicU64::new(1),
            config,
            wal: None,
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            messages_published_total: std::sync::atomic::AtomicU64::new(0),
            messages_delivered_total: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Create a broker that uses the provided write-ahead log for durable
    /// operations such as `publish_durable` and WAL-based recovery.
    #[inline(always)]
    pub fn new_with_wal(config: BrokerConfig, wal: Arc<WriteAheadLog>) -> Self {
        Self {
            topics: TopicShards::new(16),
            subscriptions: RwLock::new(HashMap::new()),
            next_subscription_id: AtomicU64::new(1),
            config,
            wal: Some(wal),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            messages_published_total: AtomicU64::new(0),
            messages_delivered_total: AtomicU64::new(0),
        }
    }

    pub fn config(&self) -> &BrokerConfig {
        &self.config
    }

    pub fn has_wal(&self) -> bool {
        self.wal.is_some()
    }

    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    pub fn topic_count(&self) -> usize {
        self.topics.len()
    }

    pub fn subscriber_count(&self) -> usize {
        let subscriptions = self.subscriptions.read();
        subscriptions.len()
    }

    pub fn messages_published_total(&self) -> u64 {
        self.messages_published_total.load(Ordering::Relaxed)
    }

    pub fn messages_delivered_total(&self) -> u64 {
        self.messages_delivered_total.load(Ordering::Relaxed)
    }

    pub fn subscribe(
        &self,
        client_id: ClientId,
        topic: TopicName,
        qos: QoSLevel,
    ) -> SubscriptionId {
        let queue = SubscriberQueue::new(qos, self.config.per_subscriber_queue_capacity);
        let subscriber = Arc::new(Subscriber { client_id, queue });

        let topic_arc = self.topics.get_or_insert(topic.clone());

        let sub_id = SubscriptionId(self.next_subscription_id.fetch_add(1, Ordering::Relaxed));

        {
            let mut subs = topic_arc.subscribers.write();
            subs.insert(sub_id, subscriber.clone());
        }

        {
            let mut subscriptions = self.subscriptions.write();
            subscriptions.insert(sub_id, SubscriptionRef { topic, subscriber });
        }

        sub_id
    }

    #[inline(always)]
    pub fn publish(&self, topic: &TopicName, payload: Bytes, qos: QoSLevel) {
        self.publish_with_wal_id(topic, payload, qos, None);
    }

    #[inline(always)]
    #[tracing::instrument(skip(self, payload))]
    fn publish_with_wal_id(
        &self,
        topic: &TopicName,
        payload: Bytes,
        qos: QoSLevel,
        wal_id: Option<u64>,
    ) {
        if self.is_shutting_down() {
            return;
        }

        let topic = match self.topics.get(topic) {
            Some(t) => t,
            None => return,
        };

        let subscribers = topic.subscribers.read();

        for subscriber in subscribers.values() {
            subscriber
                .queue
                .enqueue(payload.clone(), qos, wal_id, Some(self.config.message_ttl));
        }
    }

    /// Publish a message durably by first appending it to the write-ahead log
    /// and then enqueuing it to relevant subscribers.
    pub async fn publish_durable(
        &self,
        topic: &TopicName,
        payload: Bytes,
        qos: QoSLevel,
    ) -> Result<u64, LogError> {
        if self.is_shutting_down() {
            return Err(LogError::Corruption(
                "write-ahead log not available: broker shutting down".to_string(),
            ));
        }
        let wal = match &self.wal {
            Some(w) => w.clone(),
            None => {
                return Err(LogError::Corruption(
                    "write-ahead log not configured for broker".to_string(),
                ))
            }
        };

        let record = WalMessageRecord {
            topic: topic.as_str().to_string(),
            qos,
            payload: payload.clone(),
        };

        let encoded = record.encode()?;
        let wal_id = wal.append(&encoded).await?;

        self.publish_with_wal_id(topic, payload, qos, Some(wal_id));

        Ok(wal_id)
    }

    /// Replay all records currently present in the WAL and enqueue them to
    /// existing subscribers. This is intended for crash recovery.
    pub async fn replay_from_wal(&self) -> Result<(), LogError> {
        let wal = match &self.wal {
            Some(w) => w.clone(),
            None => return Ok(()),
        };

        let mut processed = 0usize;
        let yield_every = 1024usize;
        wal.iterate_from_stream(1, |record| {
            processed += 1;
            let should_yield = processed % yield_every == 0;
            async move {
                let msg = WalMessageRecord::decode(&record.payload)?;
                let topic = TopicName::new(msg.topic);
                self.publish_with_wal_id(&topic, msg.payload, msg.qos, Some(record.id));
                if should_yield {
                    tokio::task::yield_now().await;
                }
                Ok(())
            }
        })
        .await?;

        Ok(())
    }

    pub fn poll(&self, sub_id: SubscriptionId) -> Option<PolledMessage> {
        let subscriptions = self.subscriptions.read();
        let sub_ref = subscriptions.get(&sub_id)?;

        let (entry, tag) = sub_ref
            .subscriber
            .queue
            .dequeue(self.config.retry_base_delay)?;

        Some(PolledMessage {
            subscription_id: sub_id,
            topic: sub_ref.topic.clone(),
            payload: entry.payload,
            qos: entry.qos,
            delivery_tag: tag,
        })
    }

    pub fn ack(&self, sub_id: SubscriptionId, tag: DeliveryTag) -> bool {
        let subscriptions = self.subscriptions.read();
        let sub_ref = match subscriptions.get(&sub_id) {
            Some(r) => r,
            None => return false,
        };

        sub_ref.subscriber.queue.ack(tag)
    }

    /// Total number of QoS1 messages currently tracked as in-flight across
    /// all subscriptions.
    pub fn inflight_message_count(&self) -> usize {
        let subscriptions = self.subscriptions.read();
        subscriptions
            .values()
            .map(|sub_ref| sub_ref.subscriber.queue.inflight_len())
            .sum()
    }

    /// Perform periodic maintenance such as TTL expiration and retry
    /// scheduling. Intended to be called from a Tokio interval in the
    /// daemon.
    pub fn maintenance_tick(&self, now: std::time::Instant) {
        let subscriptions = self.subscriptions.read();
        for sub_ref in subscriptions.values() {
            sub_ref.subscriber.queue.maintenance_tick(
                now,
                self.config.max_retries,
                self.config.retry_base_delay,
            );
        }
    }

    /// Flush the underlying WAL, if configured.
    pub async fn flush_wal(&self) -> Result<(), LogError> {
        if let Some(wal) = &self.wal {
            wal.flush().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    fn test_broker() -> Broker {
        Broker::new(BrokerConfig {
            default_qos: QoSLevel::AtLeastOnce,
            message_ttl: Duration::from_secs(60),
            per_subscriber_queue_capacity: 16,
            max_retries: 3,
            retry_base_delay: Duration::from_millis(50),
        })
    }

    #[test]
    fn subscribe_publish_poll_qos0() {
        let broker = test_broker();
        let topic = TopicName::new("test");
        let client = ClientId::new("client-1");

        let sub_id = broker.subscribe(client, topic.clone(), QoSLevel::AtMostOnce);

        let payload = Bytes::from_static(b"hello");
        broker.publish(&topic, payload.clone(), QoSLevel::AtMostOnce);

        let msg = broker.poll(sub_id).expect("expected one message");
        assert_eq!(msg.payload, payload);
        assert_eq!(msg.qos, QoSLevel::AtMostOnce);
        assert!(msg.delivery_tag.is_none());

        // Queue should now be empty.
        assert!(broker.poll(sub_id).is_none());
    }

    #[test]
    fn subscribe_publish_poll_ack_qos1() {
        let broker = test_broker();
        let topic = TopicName::new("test-qos1");
        let client = ClientId::new("client-1");

        let sub_id = broker.subscribe(client, topic.clone(), QoSLevel::AtLeastOnce);

        let payload = Bytes::from_static(b"important");
        broker.publish(&topic, payload.clone(), QoSLevel::AtLeastOnce);

        let msg = broker.poll(sub_id).expect("expected one message");
        assert_eq!(msg.payload, payload);
        assert_eq!(msg.qos, QoSLevel::AtLeastOnce);
        let tag = msg.delivery_tag.expect("expected delivery tag for QoS1");

        // Without ack there should be no duplicate yet.
        assert!(broker.poll(sub_id).is_none());

        assert!(broker.ack(sub_id, tag));
        // Acking again should return false.
        assert!(!broker.ack(sub_id, tag));
    }

    #[test]
    fn multiple_subscribers_receive_same_message() {
        let broker = test_broker();
        let topic = TopicName::new("broadcast");

        let client1 = ClientId::new("c1");
        let client2 = ClientId::new("c2");

        let sub1 = broker.subscribe(client1, topic.clone(), QoSLevel::AtMostOnce);
        let sub2 = broker.subscribe(client2, topic.clone(), QoSLevel::AtLeastOnce);

        let payload = Bytes::from_static(b"fanout");
        broker.publish(&topic, payload.clone(), QoSLevel::AtLeastOnce);

        let msg1 = broker
            .poll(sub1)
            .expect("subscriber 1 should receive message");
        let msg2 = broker
            .poll(sub2)
            .expect("subscriber 2 should receive message");

        assert_eq!(msg1.payload, payload);
        assert_eq!(msg2.payload, payload);

        // QoS0 delivery has no tag.
        assert!(msg1.delivery_tag.is_none());
        // QoS1 has a tag that can be acked.
        let tag2 = msg2.delivery_tag.expect("expected tag for QoS1");
        assert!(broker.ack(sub2, tag2));
    }

    #[test]
    fn ttl_expiration_drops_messages() {
        let broker = Broker::new(BrokerConfig {
            default_qos: QoSLevel::AtLeastOnce,
            message_ttl: Duration::from_millis(50),
            per_subscriber_queue_capacity: 16,
            max_retries: 3,
            retry_base_delay: Duration::from_millis(10),
        });

        let topic = TopicName::new("ttl-test");
        let client = ClientId::new("client-ttl");
        let sub_id = broker.subscribe(client, topic.clone(), QoSLevel::AtLeastOnce);

        let payload = Bytes::from_static(b"ttl-message");
        broker.publish(&topic, payload.clone(), QoSLevel::AtLeastOnce);

        // Immediately after publish, the message is available.
        let msg = broker.poll(sub_id).expect("message should be available");
        assert_eq!(msg.payload, payload);

        // No ack: message is now inflight; next poll should return None.
        assert!(broker.poll(sub_id).is_none());

        // Wait for TTL to expire and run maintenance; the inflight message
        // should be dropped and not redelivered.
        std::thread::sleep(Duration::from_millis(70));
        broker.maintenance_tick(Instant::now());

        assert!(broker.poll(sub_id).is_none(), "message should have expired");
    }

    #[test]
    fn qos1_retry_after_timeout() {
        let broker = Broker::new(BrokerConfig {
            default_qos: QoSLevel::AtLeastOnce,
            message_ttl: Duration::from_secs(5),
            per_subscriber_queue_capacity: 16,
            max_retries: 3,
            retry_base_delay: Duration::from_millis(20),
        });

        let topic = TopicName::new("retry-test");
        let client = ClientId::new("client-retry");
        let sub_id = broker.subscribe(client, topic.clone(), QoSLevel::AtLeastOnce);

        let payload = Bytes::from_static(b"retry-message");
        broker.publish(&topic, payload.clone(), QoSLevel::AtLeastOnce);

        // First delivery.
        let msg1 = broker.poll(sub_id).expect("expected initial delivery");
        assert_eq!(msg1.payload, payload);
        let tag1 = msg1.delivery_tag.expect("QoS1 delivery should carry a tag");

        // Without ack there should be no immediate redelivery.
        assert!(broker.poll(sub_id).is_none());

        // After the retry delay, the message should be redelivered.
        std::thread::sleep(Duration::from_millis(50));
        broker.maintenance_tick(Instant::now());

        let msg2 = broker
            .poll(sub_id)
            .expect("message should be redelivered after retry delay");
        assert_eq!(msg2.payload, payload);
        let tag2 = msg2
            .delivery_tag
            .expect("redelivered message should also carry a tag");

        // Tags may or may not match depending on internal implementation, but
        // both must be ackable.
        assert!(broker.ack(sub_id, tag2));
        // After ack, there should be no further deliveries.
        broker.maintenance_tick(Instant::now());
        assert!(broker.poll(sub_id).is_none());
        // Original tag should no longer be valid.
        assert!(!broker.ack(sub_id, tag1));
    }

    #[tokio::test]
    async fn durable_messages_survive_crash_and_recovery() {
        let mut path = std::env::temp_dir();
        path.push("core_durable_test.log");
        let _ = std::fs::remove_file(&path);

        let wal = Arc::new(
            WriteAheadLog::open(&path)
                .await
                .expect("failed to open WAL"),
        );

        let config = BrokerConfig {
            default_qos: QoSLevel::AtLeastOnce,
            message_ttl: Duration::from_secs(60),
            per_subscriber_queue_capacity: 16,
            max_retries: 3,
            retry_base_delay: Duration::from_millis(50),
        };

        let broker1 = Broker::new_with_wal(config.clone(), wal.clone());

        let topic = TopicName::new("durable");
        let client1 = ClientId::new("client-1");
        let sub1 = broker1.subscribe(client1, topic.clone(), QoSLevel::AtLeastOnce);

        let payload = Bytes::from_static(b"durable-hello");
        broker1
            .publish_durable(&topic, payload.clone(), QoSLevel::AtLeastOnce)
            .await
            .expect("durable publish failed");

        // Message is available before "crash".
        let msg = broker1.poll(sub1).expect("expected message before crash");
        assert_eq!(msg.payload, payload);

        // Simulate crash by dropping the broker but keeping the WAL.
        drop(broker1);

        // Recreate broker and subscriber, then replay WAL.
        let broker2 = Broker::new_with_wal(config, wal.clone());
        let client2 = ClientId::new("client-2");
        let sub2 = broker2.subscribe(client2, topic.clone(), QoSLevel::AtLeastOnce);

        broker2
            .replay_from_wal()
            .await
            .expect("replay from WAL failed");

        let recovered = broker2.poll(sub2).expect("expected message after recovery");
        assert_eq!(recovered.payload, payload);
        assert_eq!(recovered.qos, QoSLevel::AtLeastOnce);
    }
}
