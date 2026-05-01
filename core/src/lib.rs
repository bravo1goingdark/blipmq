use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use parking_lot::{Mutex, RwLock};
use smallvec::SmallVec;
use wal::{WalError as LogError, WalRecord, WriteAheadLog};

/// Push channel type used to forward `DeliveryHandle`s from the broker hot
/// path to a connection's writer task. We use `flume` instead of
/// `tokio::sync::mpsc` because the per-op overhead is materially lower
/// (measured ~3-5x faster on `try_send`/`try_recv` workloads) and it has
/// the same single-consumer / multi-producer shape we need. `flume`'s
/// `Receiver::recv_async` integrates cleanly with tokio's runtime.
pub type PushSender = flume::Sender<DeliveryHandle>;
pub type PushReceiver = flume::Receiver<DeliveryHandle>;

/// Inline capacity for the per-publish subscriber snapshot. Most topics in
/// the wild have a handful of subscribers; sizing the inline buffer at 16
/// keeps the small case allocation-free, while larger fanouts spill to the
/// heap.
type SubscriberSnapshot<'a> = SmallVec<[(SubscriptionId, Arc<Subscriber>); 16]>;

/// A single delivery flowing from the broker to a subscriber's connection
/// writer task. The push path (`subscribe_with_conn`) is what enables v2
/// server-initiated DELIVER frames; for v1 poll-only subscribers, this
/// type is unused.
#[derive(Debug, Clone)]
pub struct DeliveryHandle {
    pub subscription_id: SubscriptionId,
    pub topic: TopicName,
    pub payload: Bytes,
    pub qos: QoSLevel,
    /// 0 for QoS0 (fire-and-forget); broker-assigned tag for QoS1.
    pub delivery_tag: u64,
}

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

/// Interned topic name. Cloning a `TopicName` is a refcount bump, not a heap
/// allocation — important on the publish hot path where the name is cloned
/// once per delivered message.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TopicName(Arc<str>);

impl TopicName {
    pub fn new<S: Into<String>>(name: S) -> Self {
        let s: String = name.into();
        Self(Arc::from(s))
    }

    /// Construct from a `&str` without a `String` round-trip.
    pub fn from_str(s: &str) -> Self {
        Self(Arc::from(s))
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
    /// Sharded subscriptions map, indexed by `sub_id & mask`. Each shard is
    /// a small `RwLock<HashMap<...>>`; sharding removes the global write-lock
    /// contention that today serializes all subscribe/unsubscribe calls.
    subscriptions: SubscriptionShards,
    /// Tracks which SubscriptionIds belong to which network connection so
    /// `unsubscribe_connection` can drop them all when the conn drops.
    /// Only push subscriptions (created via `subscribe_with_conn`) are
    /// tracked here.
    connection_subs: RwLock<HashMap<u64, Vec<SubscriptionId>>>,
    next_subscription_id: AtomicU64,
    wal: Option<Arc<WriteAheadLog>>,
    shutting_down: std::sync::atomic::AtomicBool,
    messages_published_total: std::sync::atomic::AtomicU64,
    messages_delivered_total: std::sync::atomic::AtomicU64,
    /// Total messages dropped due to a slow push consumer (mpsc Sender::try_send
    /// returned Full). Surfaced to metrics; in Phase 6 this drives the
    /// slow-consumer policy.
    push_dropped_total: std::sync::atomic::AtomicU64,
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
        // ahash is ~3-5× faster than std's SipHash for short keys, and is the
        // standard fast-but-still-secure choice. We use a fixed seed so
        // shard placement is deterministic across runs (simplifies ops and
        // testing); ahash with fixed seed is fine for non-adversarial keys.
        let mut hasher = ahash::AHasher::default();
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

/// Sharded subscription registry. SubscriptionId comes from a monotonic
/// `AtomicU64`, so a low-bits mask yields uniform placement across shards.
#[derive(Debug)]
struct SubscriptionShards {
    shards: Vec<RwLock<HashMap<SubscriptionId, SubscriptionRef>>>,
    mask: usize,
}

impl SubscriptionShards {
    fn new(num_shards: usize) -> Self {
        // Round up to next power of two so we can use a bitmask instead of
        // a modulo. 16 by default — small enough for low memory cost,
        // big enough to reduce write-lock contention under typical load.
        let n = num_shards.max(1).next_power_of_two();
        let mut shards = Vec::with_capacity(n);
        for _ in 0..n {
            shards.push(RwLock::new(HashMap::new()));
        }
        Self {
            shards,
            mask: n - 1,
        }
    }

    #[inline(always)]
    fn shard_for(&self, sub_id: SubscriptionId) -> &RwLock<HashMap<SubscriptionId, SubscriptionRef>> {
        &self.shards[(sub_id.value() as usize) & self.mask]
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
    /// When `Some`, this is a v2 push subscription: enqueue forwards
    /// directly to this Sender (no Mutex on the QoS0 path) and the
    /// connection's writer task drains the corresponding Receiver.
    /// When `None`, this is a v1 poll subscription using `queue` only.
    push_sender: Option<PushSender>,
    /// Lock-free monotonic delivery-tag source for the push path. Decoupled
    /// from `SubscriberQueueInner::next_tag` so QoS0 push enqueue takes no
    /// mutex.
    push_next_tag: AtomicU64,
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
    pending: VecDeque<DeliveryTag>,
    pending_entries: HashMap<DeliveryTag, QueueEntry>,
    inflight: HashMap<DeliveryTag, QueueEntry>,
    expiration_heap: BinaryHeap<Reverse<ExpirationEntry>>,
    retry_heap: BinaryHeap<Reverse<RetryEntry>>,
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

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct ExpirationEntry {
    expires_at: std::time::Instant,
    tag: DeliveryTag,
}

impl Ord for ExpirationEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.expires_at
            .cmp(&other.expires_at)
            .then_with(|| self.tag.value().cmp(&other.tag.value()))
    }
}

impl PartialOrd for ExpirationEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct RetryEntry {
    next_delivery_at: std::time::Instant,
    tag: DeliveryTag,
}

impl Ord for RetryEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.next_delivery_at
            .cmp(&other.next_delivery_at)
            .then_with(|| self.tag.value().cmp(&other.tag.value()))
    }
}

impl PartialOrd for RetryEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone)]
struct WalMessageRecord {
    topic: String,
    qos: QoSLevel,
    payload: Bytes,
}

impl WalMessageRecord {
    fn encode(&self) -> Result<Bytes, LogError> {
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

        Ok(buf.freeze())
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
                pending_entries: HashMap::new(),
                inflight: HashMap::new(),
                expiration_heap: BinaryHeap::new(),
                retry_heap: BinaryHeap::new(),
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
        while inner.pending_entries.len() + inner.inflight.len() >= self.capacity {
            if let Some(tag) = inner.pending.pop_front() {
                inner.pending_entries.remove(&tag);
            } else {
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

        let entry = QueueEntry {
            tag,
            payload,
            qos: effective_qos,
            wal_id,
            created_at: now,
            ttl,
            delivery_attempts: 0,
            next_delivery_at: now,
        };

        if let Some(ttl) = entry.ttl {
            inner.expiration_heap.push(Reverse(ExpirationEntry {
                expires_at: entry.created_at + ttl,
                tag: entry.tag,
            }));
        }

        inner.pending.push_back(entry.tag);
        inner.pending_entries.insert(entry.tag, entry);
    }

    /// Register a QoS1 message dispatched via the push path as inflight, so
    /// `ack` and the retry/expiration heaps work uniformly with the poll
    /// path. Called from the broker hot path; deliberately small.
    #[inline(always)]
    fn register_push_inflight(
        &self,
        tag: DeliveryTag,
        payload: Bytes,
        wal_id: Option<u64>,
        ttl: Option<Duration>,
    ) {
        let now = std::time::Instant::now();
        let entry = QueueEntry {
            tag,
            payload,
            qos: QoSLevel::AtLeastOnce,
            wal_id,
            created_at: now,
            ttl,
            delivery_attempts: 1,
            next_delivery_at: now,
        };

        let mut inner = self.inner.lock();
        if let Some(ttl) = entry.ttl {
            inner.expiration_heap.push(Reverse(ExpirationEntry {
                expires_at: entry.created_at + ttl,
                tag: entry.tag,
            }));
        }
        inner.inflight.insert(entry.tag, entry);
    }

    /// Roll back a `register_push_inflight` when the corresponding `try_send`
    /// fails. Cheap: drops the inflight entry; the heap entry becomes a
    /// tombstone the next maintenance tick will skip.
    #[inline(always)]
    fn cancel_push_inflight(&self, tag: DeliveryTag) {
        let mut inner = self.inner.lock();
        inner.inflight.remove(&tag);
    }

    #[inline(always)]
    fn dequeue(&self, base_delay: Duration) -> Option<(QueueEntry, Option<DeliveryTag>)> {
        let mut inner = self.inner.lock();
        let mut entry = loop {
            let tag = inner.pending.pop_front()?;
            if let Some(entry) = inner.pending_entries.remove(&tag) {
                break entry;
            }
        };

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
                inner.retry_heap.push(Reverse(RetryEntry {
                    next_delivery_at: entry.next_delivery_at,
                    tag: entry.tag,
                }));
                Some((entry, tag))
            }
        }
    }

    #[allow(dead_code)]
    fn peek(&self) -> Option<Bytes> {
        let inner = self.inner.lock();
        inner
            .pending
            .iter()
            .find_map(|tag| inner.pending_entries.get(tag).map(|e| e.payload.clone()))
    }

    fn ack(&self, tag: DeliveryTag) -> bool {
        let mut inner = self.inner.lock();
        inner.inflight.remove(&tag).is_some()
    }

    fn inflight_len(&self) -> usize {
        let inner = self.inner.lock();
        inner.inflight.len()
    }

    fn expiration_heap_len(&self) -> usize {
        let inner = self.inner.lock();
        inner.expiration_heap.len()
    }

    fn retry_heap_len(&self) -> usize {
        let inner = self.inner.lock();
        inner.retry_heap.len()
    }

    fn maintenance_tick(&self, now: std::time::Instant, max_retries: u32, _base_delay: Duration) {
        let mut inner = self.inner.lock();

        while let Some(Reverse(expiration)) = inner.expiration_heap.peek().copied() {
            if expiration.expires_at > now {
                break;
            }
            inner.expiration_heap.pop();

            if let Some(entry) = inner.pending_entries.get(&expiration.tag) {
                if entry
                    .ttl
                    .map(|ttl| entry.created_at + ttl == expiration.expires_at)
                    .unwrap_or(false)
                {
                    inner.pending_entries.remove(&expiration.tag);
                }
                continue;
            }

            if let Some(entry) = inner.inflight.get(&expiration.tag) {
                if entry
                    .ttl
                    .map(|ttl| entry.created_at + ttl == expiration.expires_at)
                    .unwrap_or(false)
                {
                    inner.inflight.remove(&expiration.tag);
                }
            }
        }

        while let Some(Reverse(retry)) = inner.retry_heap.peek().copied() {
            if retry.next_delivery_at > now {
                break;
            }
            inner.retry_heap.pop();

            let Some(entry) = inner.inflight.get(&retry.tag) else {
                continue;
            };

            if entry.next_delivery_at != retry.next_delivery_at {
                continue;
            }

            if entry.delivery_attempts >= max_retries {
                inner.inflight.remove(&retry.tag);
                continue;
            }

            if let Some(mut entry) = inner.inflight.remove(&retry.tag) {
                entry.next_delivery_at = now;
                inner.pending.push_back(entry.tag);
                inner.pending_entries.insert(entry.tag, entry);
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
            subscriptions: SubscriptionShards::new(16),
            connection_subs: RwLock::new(HashMap::new()),
            next_subscription_id: AtomicU64::new(1),
            config,
            wal: None,
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            messages_published_total: std::sync::atomic::AtomicU64::new(0),
            messages_delivered_total: std::sync::atomic::AtomicU64::new(0),
            push_dropped_total: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Create a broker that uses the provided write-ahead log for durable
    /// operations such as `publish_durable` and WAL-based recovery.
    #[inline(always)]
    pub fn new_with_wal(config: BrokerConfig, wal: Arc<WriteAheadLog>) -> Self {
        Self {
            topics: TopicShards::new(16),
            subscriptions: SubscriptionShards::new(16),
            connection_subs: RwLock::new(HashMap::new()),
            next_subscription_id: AtomicU64::new(1),
            config,
            wal: Some(wal),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            messages_published_total: AtomicU64::new(0),
            messages_delivered_total: AtomicU64::new(0),
            push_dropped_total: AtomicU64::new(0),
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
        self.subscriptions.len()
    }

    pub fn messages_published_total(&self) -> u64 {
        self.messages_published_total.load(Ordering::Relaxed)
    }

    pub fn messages_delivered_total(&self) -> u64 {
        self.messages_delivered_total.load(Ordering::Relaxed)
    }

    /// Subscribe in poll mode (v1). The subscriber's messages accumulate in
    /// the per-subscriber queue and are pulled out via [`Broker::poll`]. No
    /// connection-lifetime tracking; the subscription stays until the broker
    /// is dropped.
    pub fn subscribe(
        &self,
        client_id: ClientId,
        topic: TopicName,
        qos: QoSLevel,
    ) -> SubscriptionId {
        self.subscribe_inner(client_id, topic, qos, None, None)
    }

    /// Subscribe in push mode (v2). Messages are forwarded directly into the
    /// provided mpsc channel; no per-message Mutex is taken on the QoS0 hot
    /// path. The subscription is tied to `conn_id` so a connection drop can
    /// be cleaned up wholesale via [`Broker::unsubscribe_connection`].
    pub fn subscribe_with_conn(
        &self,
        client_id: ClientId,
        topic: TopicName,
        qos: QoSLevel,
        conn_id: u64,
        push_sender: PushSender,
    ) -> SubscriptionId {
        self.subscribe_inner(client_id, topic, qos, Some(conn_id), Some(push_sender))
    }

    fn subscribe_inner(
        &self,
        client_id: ClientId,
        topic: TopicName,
        qos: QoSLevel,
        conn_id: Option<u64>,
        push_sender: Option<PushSender>,
    ) -> SubscriptionId {
        let queue = SubscriberQueue::new(qos, self.config.per_subscriber_queue_capacity);
        let subscriber = Arc::new(Subscriber {
            client_id,
            queue,
            push_sender,
            push_next_tag: AtomicU64::new(1),
        });

        let topic_arc = self.topics.get_or_insert(topic.clone());

        let sub_id = SubscriptionId(self.next_subscription_id.fetch_add(1, Ordering::Relaxed));

        {
            let mut subs = topic_arc.subscribers.write();
            subs.insert(sub_id, subscriber.clone());
        }

        {
            let mut shard = self.subscriptions.shard_for(sub_id).write();
            shard.insert(sub_id, SubscriptionRef { topic, subscriber });
        }

        if let Some(c) = conn_id {
            let mut conn_map = self.connection_subs.write();
            conn_map.entry(c).or_default().push(sub_id);
        }

        sub_id
    }

    /// Drop every subscription registered against `conn_id`. Called by the
    /// network layer when a connection closes so that we don't leak
    /// subscriber state and the writer task's Receiver wakes (because the
    /// last Sender is dropped).
    pub fn unsubscribe_connection(&self, conn_id: u64) {
        let sub_ids: Vec<SubscriptionId> = {
            let mut conn_map = self.connection_subs.write();
            conn_map.remove(&conn_id).unwrap_or_default()
        };

        if sub_ids.is_empty() {
            return;
        }

        // Remove from the sharded subscriptions map first so concurrent
        // publishers stop seeing them. Each sub_id touches exactly one
        // shard; we acquire write locks lazily per affected shard.
        let mut removed: Vec<(SubscriptionId, SubscriptionRef)> = Vec::with_capacity(sub_ids.len());
        for sid in &sub_ids {
            let mut shard = self.subscriptions.shard_for(*sid).write();
            if let Some(sub_ref) = shard.remove(sid) {
                removed.push((*sid, sub_ref));
            }
        }

        // Then remove from each per-topic subscriber map.
        for (sid, sub_ref) in &removed {
            if let Some(topic) = self.topics.get(&sub_ref.topic) {
                let mut topic_subs = topic.subscribers.write();
                topic_subs.remove(sid);
            }
        }
        // `removed` going out of scope drops the only remaining strong
        // references to those Subscribers (and thus their push_senders),
        // which closes the connection's push channel.
    }

    /// Total number of QoS0/QoS1 deliveries dropped because a push subscriber's
    /// channel was full. See [`BrokerConfig::per_subscriber_queue_capacity`].
    pub fn push_dropped_total(&self) -> u64 {
        self.push_dropped_total.load(Ordering::Relaxed)
    }

    #[inline(always)]
    pub fn publish(&self, topic: &TopicName, payload: Bytes, qos: QoSLevel) {
        self.publish_with_wal_id(topic, payload, qos, None);
    }

    #[inline(always)]
    #[tracing::instrument(skip(self, payload))]
    fn publish_with_wal_id(
        &self,
        topic_name: &TopicName,
        payload: Bytes,
        qos: QoSLevel,
        wal_id: Option<u64>,
    ) {
        if self.is_shutting_down() {
            return;
        }

        let topic = match self.topics.get(topic_name) {
            Some(t) => t,
            None => return,
        };

        // Snapshot the subscriber list under a brief read lock. With many
        // subscribers, holding the lock across the whole fanout would
        // serialize against subscribe/unsubscribe; copying Arc pointers is
        // a cheap refcount bump.
        let snapshot: SubscriberSnapshot<'_> = {
            let subscribers = topic.subscribers.read();
            subscribers.iter().map(|(id, sub)| (*id, sub.clone())).collect()
        };

        for (sub_id, subscriber) in snapshot.iter() {
            if let Some(sender) = &subscriber.push_sender {
                // Push fast path. QoS0: zero locks (atomic tag + try_send).
                // QoS1: still atomic tag + try_send, plus a brief inner lock
                // to register inflight for ack/retry tracking.
                let tag = subscriber.push_next_tag.fetch_add(1, Ordering::Relaxed);

                if qos == QoSLevel::AtLeastOnce {
                    // Track inflight for ack/retry. Reuses the existing inner
                    // mutex; this lock is per-subscriber and only contended
                    // by ack/maintenance, never by other publishers.
                    subscriber.queue.register_push_inflight(
                        DeliveryTag(tag),
                        payload.clone(),
                        wal_id,
                        Some(self.config.message_ttl),
                    );
                }

                let handle = DeliveryHandle {
                    subscription_id: *sub_id,
                    topic: topic_name.clone(),
                    payload: payload.clone(),
                    qos,
                    delivery_tag: if qos == QoSLevel::AtLeastOnce { tag } else { 0 },
                };

                if sender.try_send(handle).is_err() {
                    self.push_dropped_total.fetch_add(1, Ordering::Relaxed);
                    if qos == QoSLevel::AtLeastOnce {
                        // Roll back the inflight registration we just made,
                        // since the message will not be delivered.
                        subscriber.queue.cancel_push_inflight(DeliveryTag(tag));
                    }
                }
            } else {
                // Poll path (v1): unchanged.
                subscriber
                    .queue
                    .enqueue(payload.clone(), qos, wal_id, Some(self.config.message_ttl));
            }
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
        // append_durable returns only after fsync covers this record. This
        // is what makes "publish_durable returned Ok" mean "on disk".
        let wal_id = wal.append_durable(encoded).await?;

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

        let records: Vec<WalRecord> = wal.iterate_from(1).await?;
        for record in records {
            let msg = WalMessageRecord::decode(&record.payload)?;
            let topic = TopicName::new(msg.topic);
            self.publish_with_wal_id(&topic, msg.payload, msg.qos, Some(record.id));
        }

        Ok(())
    }

    pub fn poll(&self, sub_id: SubscriptionId) -> Option<PolledMessage> {
        let shard = self.subscriptions.shard_for(sub_id).read();
        let sub_ref = shard.get(&sub_id)?;

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
        let shard = self.subscriptions.shard_for(sub_id).read();
        let sub_ref = match shard.get(&sub_id) {
            Some(r) => r,
            None => return false,
        };

        sub_ref.subscriber.queue.ack(tag)
    }

    /// Total number of QoS1 messages currently tracked as in-flight across
    /// all subscriptions.
    pub fn inflight_message_count(&self) -> usize {
        let mut total = 0;
        for shard in &self.subscriptions.shards {
            for sub_ref in shard.read().values() {
                total += sub_ref.subscriber.queue.inflight_len();
            }
        }
        total
    }

    pub fn expiration_heap_size(&self) -> usize {
        let mut total = 0;
        for shard in &self.subscriptions.shards {
            for sub_ref in shard.read().values() {
                total += sub_ref.subscriber.queue.expiration_heap_len();
            }
        }
        total
    }

    pub fn retry_heap_size(&self) -> usize {
        let mut total = 0;
        for shard in &self.subscriptions.shards {
            for sub_ref in shard.read().values() {
                total += sub_ref.subscriber.queue.retry_heap_len();
            }
        }
        total
    }

    /// Perform periodic maintenance such as TTL expiration and retry
    /// scheduling. Intended to be called from a Tokio interval in the
    /// daemon.
    pub fn maintenance_tick(&self, now: std::time::Instant) {
        for shard in &self.subscriptions.shards {
            for sub_ref in shard.read().values() {
                sub_ref.subscriber.queue.maintenance_tick(
                    now,
                    self.config.max_retries,
                    self.config.retry_base_delay,
                );
            }
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

    #[tokio::test]
    async fn push_subscribe_delivers_qos0_without_poll() {
        let broker = test_broker();
        let topic = TopicName::new("push-q0");
        let (tx, rx) = flume::bounded::<DeliveryHandle>(64);

        let sub_id = broker.subscribe_with_conn(
            ClientId::new("push-c1"),
            topic.clone(),
            QoSLevel::AtMostOnce,
            42, // conn_id
            tx,
        );

        let payload = Bytes::from_static(b"push-hello");
        broker.publish(&topic, payload.clone(), QoSLevel::AtMostOnce);

        let handle = rx.recv_async().await.expect("expected one delivery");
        assert_eq!(handle.subscription_id, sub_id);
        assert_eq!(handle.payload, payload);
        assert_eq!(handle.qos, QoSLevel::AtMostOnce);
        assert_eq!(handle.delivery_tag, 0); // QoS0 has no tag

        // Push subscribers should NOT also queue for poll.
        assert!(broker.poll(sub_id).is_none(),
            "push subscriber must not have a pending poll-path entry");
    }

    #[tokio::test]
    async fn push_subscribe_delivers_qos1_with_inflight_tracking() {
        let broker = test_broker();
        let topic = TopicName::new("push-q1");
        let (tx, rx) = flume::bounded::<DeliveryHandle>(64);

        let sub_id = broker.subscribe_with_conn(
            ClientId::new("push-c1"),
            topic.clone(),
            QoSLevel::AtLeastOnce,
            7,
            tx,
        );

        let payload = Bytes::from_static(b"durable-push");
        broker.publish(&topic, payload.clone(), QoSLevel::AtLeastOnce);

        let handle = rx.recv_async().await.expect("expected delivery");
        assert!(handle.delivery_tag != 0, "QoS1 must carry a non-zero tag");
        assert_eq!(broker.inflight_message_count(), 1);

        // Acking the delivery_tag should clear inflight.
        assert!(broker.ack(sub_id, DeliveryTag::from_raw(handle.delivery_tag)));
        assert_eq!(broker.inflight_message_count(), 0);
    }

    #[tokio::test]
    async fn unsubscribe_connection_drops_all_subs_and_closes_channel() {
        let broker = test_broker();
        let topic_a = TopicName::new("conn-a");
        let topic_b = TopicName::new("conn-b");
        let (tx, rx) = flume::bounded::<DeliveryHandle>(64);

        let conn_id = 99u64;
        let _sub_a = broker.subscribe_with_conn(
            ClientId::new("c1"),
            topic_a.clone(),
            QoSLevel::AtMostOnce,
            conn_id,
            tx.clone(),
        );
        let _sub_b = broker.subscribe_with_conn(
            ClientId::new("c1"),
            topic_b.clone(),
            QoSLevel::AtMostOnce,
            conn_id,
            tx,
        );
        assert_eq!(broker.subscriber_count(), 2);

        broker.unsubscribe_connection(conn_id);
        assert_eq!(broker.subscriber_count(), 0);

        // Subsequent publishes go nowhere; the receiver sees the channel
        // close because all senders have been dropped (flume returns
        // RecvError::Disconnected once the last Sender goes away).
        broker.publish(&topic_a, Bytes::from_static(b"x"), QoSLevel::AtMostOnce);
        broker.publish(&topic_b, Bytes::from_static(b"y"), QoSLevel::AtMostOnce);
        assert!(rx.recv_async().await.is_err(),
            "channel should close once last sender is dropped");
    }

    #[tokio::test]
    async fn push_full_channel_increments_dropped_counter() {
        let broker = test_broker();
        let topic = TopicName::new("push-full");
        let (tx, rx) = flume::bounded::<DeliveryHandle>(2); // tiny capacity

        let _sub = broker.subscribe_with_conn(
            ClientId::new("c1"),
            topic.clone(),
            QoSLevel::AtMostOnce,
            1,
            tx,
        );

        for _ in 0..10 {
            broker.publish(&topic, Bytes::from_static(b"x"), QoSLevel::AtMostOnce);
        }

        // The channel held 2; the rest were dropped.
        let mut received = 0;
        while rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, 2);
        assert_eq!(broker.push_dropped_total(), 8);
    }
}
