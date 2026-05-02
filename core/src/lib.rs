use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use crc32fast::Hasher as Crc32Hasher;
use crossbeam_utils::CachePadded;
use hdrhistogram::Histogram;
use parking_lot::{Mutex, RwLock};

/// Take a latency sample 1 publish in N. Power of two so the test is
/// `count & MASK == 0`. 64 keeps p99 estimates accurate to within
/// ~hundreds of microseconds at 1 M+ publishes/sec, while removing
/// the histogram lock from 63/64 of publishes.
const PUBLISH_LATENCY_SAMPLE_INTERVAL: u64 = 64;
const PUBLISH_LATENCY_SAMPLE_MASK: u64 = PUBLISH_LATENCY_SAMPLE_INTERVAL - 1;

/// Number of shards in `ShardedCounter`. Power of two; 16 covers the
/// publisher-thread fanout we see in benches (1..16 pipes) without
/// blowing memory. Each shard sits on its own cache line so concurrent
/// `add` calls from different threads don't ping the same line.
const COUNTER_SHARDS: usize = 16;
const COUNTER_SHARD_MASK: usize = COUNTER_SHARDS - 1;

/// Per-thread shard index. First touch on a thread reserves a slot via
/// the static `NEXT_SHARD` counter; subsequent reads are a single TLS
/// load. Threads above `COUNTER_SHARDS` wrap, which only adds light
/// contention rather than a hard cap on concurrency.
fn thread_shard_idx() -> usize {
    thread_local! {
        static SHARD_IDX: usize = {
            static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);
            NEXT_SHARD.fetch_add(1, Ordering::Relaxed) & COUNTER_SHARD_MASK
        };
    }
    SHARD_IDX.with(|i| *i)
}

/// Counter sharded across `COUNTER_SHARDS` cache-line-padded atomics so
/// concurrent publishers from different threads do not contend on the
/// same line. Reads sum across all shards (only metrics scrape paths
/// hit this, so the linear scan is not on the hot path).
#[derive(Debug, Default)]
pub struct ShardedCounter {
    shards: [CachePadded<AtomicU64>; COUNTER_SHARDS],
}

impl ShardedCounter {
    pub const fn new() -> Self {
        // `CachePadded::new` is const, but array-init via const requires
        // a more verbose path. Use a const helper.
        const fn z() -> CachePadded<AtomicU64> {
            CachePadded::new(AtomicU64::new(0))
        }
        Self {
            shards: [
                z(),
                z(),
                z(),
                z(),
                z(),
                z(),
                z(),
                z(),
                z(),
                z(),
                z(),
                z(),
                z(),
                z(),
                z(),
                z(),
            ],
        }
    }

    #[inline]
    pub fn add(&self, n: u64) -> u64 {
        let idx = thread_shard_idx();
        // Returns the per-shard previous value, plus n is the new local
        // count — sufficient for sample-mod-N gating since each thread
        // has its own monotonic stream.
        self.shards[idx].fetch_add(n, Ordering::Relaxed)
    }

    pub fn load(&self) -> u64 {
        self.shards.iter().map(|s| s.load(Ordering::Relaxed)).sum()
    }
}

use smallvec::SmallVec;
use tokio::sync::Notify;
use wal::{WalError as LogError, WalRecord, WriteAheadLog};
// Re-export so the network layer can pattern-match on WAL error variants
// (e.g. backpressure -> NACK 503) without taking a direct wal dependency.
pub use wal::WalError;

/// Push channel type used to forward `DeliveryHandle`s from the broker hot
/// path to a connection's writer task. Used for the **legacy push path**
/// (`subscribe_with_conn`); the newer `subscribe_with_slot` path is the
/// faster shared-buffer alternative used by the v2 wire layer.
pub type PushSender = flume::Sender<DeliveryHandle>;
pub type PushReceiver = flume::Receiver<DeliveryHandle>;

/// Shared push buffer for the v2 high-throughput delivery path. The broker
/// encodes DELIVER frames directly into `buf` under the mutex and pings
/// `notify`; the connection's writer task wakes, swaps `buf` for an empty
/// `BytesMut`, and `write_all`s the swapped buffer in one syscall.
///
/// This is the alternative to the flume-channel push path. It removes the
/// per-frame channel-op overhead (~150 ns / frame) and the writer's
/// per-frame DELIVER-encode step by collapsing both into a single
/// "encode-into-shared-buffer" step on the broker side.
#[derive(Debug)]
pub struct PushSlot {
    pub buf: Mutex<BytesMut>,
    pub notify: Notify,
}

impl PushSlot {
    pub fn new(initial_capacity: usize) -> Self {
        Self {
            buf: Mutex::new(BytesMut::with_capacity(initial_capacity)),
            notify: Notify::new(),
        }
    }
}

/// Closure type that encodes a single DELIVER frame into a `BytesMut`. The
/// broker calls this once per push delivery; the connection's writer task
/// later flushes the buffer to the wire. Defined here (rather than in
/// `net`) so corelib can drive the broker hot path without taking a net
/// dependency; the actual frame format lives in the net crate and is
/// passed in via this closure at subscribe time.
pub type DeliveryEncoder = Arc<
    dyn Fn(
            &mut BytesMut,
            /*qos*/ QoSLevel,
            /*tag*/ u64,
            /*topic*/ &str,
            /*payload*/ &Bytes,
        ) + Send
        + Sync,
>;

/// Inline capacity for the per-publish subscriber snapshot. Most topics in
/// the wild have a handful of subscribers; sizing the inline buffer at 16
/// keeps the small case allocation-free, while larger fanouts spill to the
/// heap.
type SubscriberSnapshot<'a> = SmallVec<[(SubscriptionId, Arc<Subscriber>); 16]>;

/// A NATS-style subject pattern. Tokens are `.`-separated. `*` matches a
/// single token; `>` matches one or more remaining tokens (must be the
/// last token, if present).
///
/// Examples:
/// - `orders.us.created` — exact match.
/// - `orders.us.*` — matches `orders.us.created`, `orders.us.shipped`.
/// - `orders.>` — matches any subject starting with `orders.`.
/// - `*.us.created` — matches `orders.us.created`, `users.us.created`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubjectPattern {
    /// Source string for the pattern, kept for diagnostics + display.
    raw: Arc<str>,
    /// Pre-parsed segments, one per `.`-separated token.
    segments: Vec<PatternSegment>,
    /// True if the pattern ends in `>`. When true, `segments` is the
    /// fixed prefix; the tail wildcard is not stored as a segment.
    has_tail: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PatternSegment {
    Literal(Arc<str>),
    Wildcard,
}

#[derive(Debug, thiserror::Error)]
pub enum PatternParseError {
    #[error("empty pattern")]
    Empty,
    #[error("empty token in pattern: {0:?}")]
    EmptyToken(String),
    #[error("`>` (tail wildcard) must be the last token: {0:?}")]
    TailNotLast(String),
}

impl SubjectPattern {
    /// Parse a NATS-style pattern. Returns `Err` if the pattern has empty
    /// tokens (e.g. `a..b` or starts/ends with `.`) or if `>` appears in
    /// any position other than the last.
    pub fn parse(s: &str) -> Result<Self, PatternParseError> {
        if s.is_empty() {
            return Err(PatternParseError::Empty);
        }
        let mut segments = Vec::new();
        let mut has_tail = false;
        let raw_tokens: Vec<&str> = s.split('.').collect();
        let last = raw_tokens.len() - 1;
        for (i, tok) in raw_tokens.iter().enumerate() {
            if tok.is_empty() {
                return Err(PatternParseError::EmptyToken(s.to_string()));
            }
            if *tok == ">" {
                if i != last {
                    return Err(PatternParseError::TailNotLast(s.to_string()));
                }
                has_tail = true;
                // Don't push the tail; it's encoded by `has_tail`.
            } else if *tok == "*" {
                segments.push(PatternSegment::Wildcard);
            } else {
                segments.push(PatternSegment::Literal(Arc::from(*tok)));
            }
        }
        Ok(Self {
            raw: Arc::from(s),
            segments,
            has_tail,
        })
    }

    /// True if this pattern contains no wildcards and matches exactly one
    /// subject. Used to short-circuit the wildcard list for exact-only
    /// subscriptions.
    pub fn is_exact(&self) -> bool {
        !self.has_tail
            && self
                .segments
                .iter()
                .all(|s| matches!(s, PatternSegment::Literal(_)))
    }

    /// The raw pattern string (for diagnostics and Prometheus labels).
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Match a concrete subject against this pattern. The subject must
    /// have no wildcards itself.
    pub fn matches(&self, subject: &str) -> bool {
        let tokens: smallvec::SmallVec<[&str; 8]> = subject.split('.').collect();
        let fixed = self.segments.len();
        if self.has_tail {
            // Fixed prefix must match; tail must be non-empty.
            if tokens.len() <= fixed {
                return false;
            }
        } else if tokens.len() != fixed {
            return false;
        }
        for (i, seg) in self.segments.iter().enumerate() {
            match seg {
                PatternSegment::Literal(lit) => {
                    if lit.as_ref() != tokens[i] {
                        return false;
                    }
                }
                PatternSegment::Wildcard => {
                    // `*` matches any single non-empty token. Tokens are
                    // produced by `split('.')` which never yields an empty
                    // token unless the subject itself has empty segments,
                    // which is invalid input the broker rejects elsewhere.
                    if tokens[i].is_empty() {
                        return false;
                    }
                }
            }
        }
        true
    }
}

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

/// What the broker does when a push subscriber's outbound buffer is over
/// the slow-consumer threshold (or — for the legacy flume push path — when
/// `try_send` returns `Full`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlowConsumerPolicy {
    /// Drop the new delivery on the floor and increment the
    /// `push_dropped_total` counter. The subscription stays alive; if
    /// the consumer catches up, deliveries resume. This is the historical
    /// default and the lowest-friction option.
    DropNewest,
    /// Drop the new delivery AND remove the subscription from the broker
    /// so further publishes don't pile up against the same slow consumer.
    /// The TCP connection itself is not closed; the client can reissue
    /// `SUBSCRIBE` if it wants to resume.
    DropSubscription,
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
    /// What to do when a push subscriber falls behind faster than the
    /// outbound buffer can drain. See [`SlowConsumerPolicy`].
    pub slow_consumer_policy: SlowConsumerPolicy,
    /// Soft byte cap on a per-conn push slot's outbound buffer. When the
    /// buffer is at or above this size, the broker applies
    /// `slow_consumer_policy` to incoming deliveries. Default 16 MiB.
    pub slow_consumer_buffer_bytes: usize,
    /// Suffix appended to a topic name when forming its dead-letter queue
    /// (DLQ) topic. When set, QoS1 messages dropped by maintenance
    /// (TTL-expired or max-retries-exceeded) are republished to
    /// `<original_topic><dlq_suffix>` at QoS0 so subscribers can react to
    /// undeliverable traffic. `None` disables the DLQ feature.
    pub dlq_suffix: Option<String>,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            default_qos: QoSLevel::AtMostOnce,
            message_ttl: Duration::from_secs(60),
            per_subscriber_queue_capacity: 1024,
            max_retries: 3,
            retry_base_delay: Duration::from_millis(50),
            slow_consumer_policy: SlowConsumerPolicy::DropNewest,
            slow_consumer_buffer_bytes: 16 * 1024 * 1024,
            dlq_suffix: Some(".dlq".to_string()),
        }
    }
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
    /// Inherent method (not `FromStr` trait) because TopicName
    /// construction is infallible and we don't want callers to write
    /// `.unwrap()` on every site. Suppressing the trait-confusion lint.
    #[allow(clippy::should_implement_trait)]
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
    /// `true` once the daemon has called `mark_ready()` (typically after WAL
    /// replay finishes). The `/readyz` probe consults this.
    ready: std::sync::atomic::AtomicBool,
    // Hot counters live in their own cache lines (CachePadded) so multi-
    // publisher fetch_adds don't ping-pong the same line across cores.
    // Each pad is typically 128 bytes on x86_64.
    messages_published_total: ShardedCounter,
    messages_delivered_total: ShardedCounter,
    /// Total messages dropped due to a slow push consumer (mpsc Sender::try_send
    /// returned Full). Surfaced to metrics; in Phase 6 this drives the
    /// slow-consumer policy.
    push_dropped_total: CachePadded<std::sync::atomic::AtomicU64>,
    /// Highest WAL id this broker has observed (via either an inbound
    /// durable publish or a replayed record). Used as the `snapshot_id`
    /// when capturing a checkpoint.
    max_wal_id_observed: CachePadded<AtomicU64>,
    /// Live `(client_id, topic) -> highest acked WAL id` cursors. Updated
    /// on every `Broker::ack` for QoS1 deliveries that had a WAL id, and
    /// captured wholesale by `current_snapshot`.
    ack_cursors: RwLock<HashMap<(String, String), u64>>,
    /// Latency histogram for the `publish_with_wal_id` fanout path,
    /// recording total publish-to-encode time in nanoseconds. The hot
    /// path samples 1-in-`PUBLISH_LATENCY_SAMPLE_INTERVAL` publishes
    /// (atomic counter + bitmask) so the typical publish never even
    /// attempts the lock. The metrics endpoint reads the same Mutex.
    publish_fanout_ns: Mutex<Histogram<u64>>,
    /// Monotonic publish counter for sample-based histogram recording.
    publish_count: ShardedCounter,
    /// Subscriptions whose pattern contains at least one wildcard. Lookup
    /// on publish is a linear scan — fine for hundreds of patterns; a
    /// later phase could replace this with a subject trie. Exact-match
    /// subscriptions stay on the `TopicShards` fast path and don't appear
    /// here.
    wildcard_subs: RwLock<Vec<(SubjectPattern, SubscriptionId)>>,
    /// Length of `wildcard_subs` mirrored as an atomic so the publish
    /// hot path can early-exit without acquiring the read lock when no
    /// wildcard subs are registered (the common case).
    wildcard_subs_len: AtomicU64,
    /// Fires whenever the broker's inflight QoS1 count could have
    /// decreased (an ack or a maintenance-tick expiry/retry-drop).
    /// `wait_for_drain` awaits this so graceful shutdown completes the
    /// instant the last in-flight clears, instead of paying a 50 ms
    /// polling slack.
    drain_notify: Notify,
}

/// A latency snapshot captured for the metrics endpoint. Times are in
/// nanoseconds; the metrics layer converts to seconds for Prometheus.
#[derive(Debug, Clone)]
pub struct LatencyStats {
    pub count: u64,
    pub sum_ns: u64,
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
}

/// Round-robin consumer group on a single topic. Members are competing
/// consumers — each `publish` to the topic fans out to non-grouped
/// subscribers as broadcast and *additionally* picks one member from
/// each registered consumer group, so a publish goes to exactly one
/// member of each group (and to all non-grouped subs).
#[derive(Debug)]
struct ConsumerGroup {
    /// Members of this group on the parent topic. Stored as a Vec so
    /// round-robin indexing is O(1). Subscribe/unsubscribe rebuild the
    /// Vec under the writer mutex; the publish path takes a brief
    /// read lock and a snapshot.
    members: RwLock<Vec<(SubscriptionId, Arc<Subscriber>)>>,
    /// Round-robin counter; bumped per publish to balance deliveries
    /// across members. `Relaxed` ordering is fine — ordering across
    /// publishers isn't promised, just that each publisher's
    /// successive publishes hit different members.
    next_idx: AtomicUsize,
}

impl ConsumerGroup {
    fn new() -> Self {
        Self {
            members: RwLock::new(Vec::new()),
            next_idx: AtomicUsize::new(0),
        }
    }
}

#[derive(Debug)]
struct Topic {
    name: TopicName,
    subscribers: RwLock<HashMap<SubscriptionId, Arc<Subscriber>>>,
    /// Named consumer groups registered on this topic. Keyed by group
    /// name. Each map entry is shared via `Arc` so the publish path
    /// can snapshot the current set under a brief read lock without
    /// holding the topic-level lock during fanout.
    consumer_groups: RwLock<HashMap<String, Arc<ConsumerGroup>>>,
    /// Number of `publish` calls that landed on this topic (one per
    /// publisher message, regardless of fanout). Sharded by publisher
    /// thread so concurrent publishes to the same topic don't ping
    /// the same cache line.
    published_total: ShardedCounter,
    /// Number of `DeliveryHandle`s emitted from fanout for this topic
    /// (i.e. published × matched-subscribers). Sharded for the same
    /// reason; the publish path flushes a batched sum once per fanout.
    delivered_total: ShardedCounter,
}

/// Cacheable handle to a resolved `Topic`. Construct via
/// [`Broker::resolve_topic`] and pass to
/// [`Broker::publish_resolved`] / [`Broker::publish_resolved_with_ttl`]
/// to skip the per-publish `TopicShards` registry lookup.
///
/// The inner `Arc<Topic>` is private; callers should treat this type
/// as opaque. `Topic` records are never removed, so a `ResolvedTopic`
/// stays valid for the broker's lifetime.
#[derive(Debug, Clone)]
pub struct ResolvedTopic {
    inner: Arc<Topic>,
}

/// Per-topic metrics snapshot, returned by `Broker::topic_metrics`.
#[derive(Debug, Clone)]
pub struct TopicMetrics {
    pub topic: TopicName,
    pub published_total: u64,
    pub delivered_total: u64,
    pub subscriber_count: usize,
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
    fn shard_for(
        &self,
        sub_id: SubscriptionId,
    ) -> &RwLock<HashMap<SubscriptionId, SubscriptionRef>> {
        &self.shards[(sub_id.value() as usize) & self.mask]
    }

    fn len(&self) -> usize {
        self.shards.iter().map(|s| s.read().len()).sum()
    }
}

struct Subscriber {
    #[allow(dead_code)]
    client_id: ClientId,
    queue: SubscriberQueue,
    /// When `Some`, this is a legacy v2-channel push subscription: enqueue
    /// forwards directly to this `flume::Sender`. Kept for tests and any
    /// caller still using the channel API.
    push_sender: Option<PushSender>,
    /// When `Some`, this is the v2 shared-buffer push subscription: the
    /// broker encodes DELIVER frames directly into `slot.buf` under the
    /// mutex and pings `slot.notify`. Used by the network layer's writer
    /// task on the hot path.
    push_slot: Option<(Arc<PushSlot>, DeliveryEncoder)>,
    /// Lock-free monotonic delivery-tag source for the push path. Decoupled
    /// from `SubscriberQueueInner::next_tag` so QoS0 push enqueue takes no
    /// mutex.
    push_next_tag: AtomicU64,
}

// Manual Debug impl: DeliveryEncoder is a `dyn Fn` and doesn't impl Debug.
impl std::fmt::Debug for Subscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscriber")
            .field("client_id", &self.client_id)
            .field("queue", &self.queue)
            .field("push_sender", &self.push_sender.is_some())
            .field("push_slot", &self.push_slot.is_some())
            .field("push_next_tag", &self.push_next_tag)
            .finish()
    }
}

#[derive(Debug)]
struct SubscriptionRef {
    topic: TopicName,
    subscriber: Arc<Subscriber>,
    /// `Some(group)` when this subscription joined a named consumer
    /// group on `topic`. The group key is what unsubscribe paths look
    /// up in `Topic.consumer_groups`. Non-grouped subscriptions land
    /// in `Topic.subscribers` (broadcast fanout) and store `None`.
    group: Option<String>,
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

/// Discriminator for WAL record payload kinds. The first byte of every
/// broker-level WAL record encoding is one of these. The on-wire WAL
/// record framing (id/len/crc/payload) is unchanged; this is purely a
/// payload-level convention so we can distinguish published-message
/// records from acknowledgement records.
const WAL_KIND_MESSAGE: u8 = 1;
const WAL_KIND_ACK: u8 = 2;

const CHECKPOINT_MAGIC: &[u8; 8] = b"BLIPCKPT";
const CHECKPOINT_VERSION: u32 = 1;

/// A point-in-time snapshot of broker recovery state, written by the
/// daemon periodically and consumed at startup. Holds, for each
/// `(client_id, topic)` consumer, the highest acked WAL id; on restart,
/// `replay_from_wal_with_checkpoint` uses these as initial cursors so it
/// only needs to walk the WAL from `snapshot_id + 1` instead of the
/// whole log. Drops O(WAL size) restart time to O(unacked tail).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointSnapshot {
    /// Largest WAL id reflected in this snapshot. Replay can skip
    /// Message records with id <= this; Ack records up to this are
    /// already aggregated into `cursors`.
    pub snapshot_id: u64,
    /// `(client_id, topic) -> highest_acked_wal_id`.
    pub cursors: HashMap<(String, String), u64>,
}

impl CheckpointSnapshot {
    pub fn empty() -> Self {
        Self {
            snapshot_id: 0,
            cursors: HashMap::new(),
        }
    }

    /// Encode to a self-describing on-disk format with a CRC32 trailer.
    pub fn encode(&self) -> Result<Bytes, LogError> {
        let mut buf = BytesMut::with_capacity(64 + self.cursors.len() * 64);
        buf.put_slice(CHECKPOINT_MAGIC);
        buf.put_u32(CHECKPOINT_VERSION);
        buf.put_u64(self.snapshot_id);
        let n = u32::try_from(self.cursors.len())
            .map_err(|_| LogError::Corruption("too many checkpoint entries".to_string()))?;
        buf.put_u32(n);
        for ((cid, topic), wal_id) in &self.cursors {
            let cid_bytes = cid.as_bytes();
            let topic_bytes = topic.as_bytes();
            let cid_len = u16::try_from(cid_bytes.len())
                .map_err(|_| LogError::Corruption("client_id too long".to_string()))?;
            let topic_len = u16::try_from(topic_bytes.len())
                .map_err(|_| LogError::Corruption("topic too long".to_string()))?;
            buf.put_u16(cid_len);
            buf.put_slice(cid_bytes);
            buf.put_u16(topic_len);
            buf.put_slice(topic_bytes);
            buf.put_u64(*wal_id);
        }
        let mut hasher = Crc32Hasher::new();
        hasher.update(&buf);
        let crc = hasher.finalize();
        buf.put_u32(crc);
        Ok(buf.freeze())
    }

    /// Decode and CRC-validate. Returns `Corruption` on any structural or
    /// CRC issue so the daemon can fall back to a full replay rather than
    /// silently use a corrupt snapshot.
    pub fn decode(bytes: &[u8]) -> Result<Self, LogError> {
        // Layout: magic(8) + version(4) + snapshot_id(8) + n(4) + entries + crc(4).
        if bytes.len() < 8 + 4 + 8 + 4 + 4 {
            return Err(LogError::Corruption(
                "checkpoint file too short".to_string(),
            ));
        }
        let crc_offset = bytes.len() - 4;
        let body = &bytes[..crc_offset];
        let mut crc_slice = &bytes[crc_offset..];
        let stored_crc = crc_slice.get_u32();
        let mut hasher = Crc32Hasher::new();
        hasher.update(body);
        if hasher.finalize() != stored_crc {
            return Err(LogError::Corruption("checkpoint CRC mismatch".to_string()));
        }

        let mut slice = body;
        let mut magic = [0u8; 8];
        slice.copy_to_slice(&mut magic);
        if &magic != CHECKPOINT_MAGIC {
            return Err(LogError::Corruption(
                "checkpoint magic mismatch".to_string(),
            ));
        }
        let version = slice.get_u32();
        if version != CHECKPOINT_VERSION {
            return Err(LogError::Corruption(format!(
                "unsupported checkpoint version: {version}"
            )));
        }
        let snapshot_id = slice.get_u64();
        let n = slice.get_u32() as usize;
        let mut cursors = HashMap::with_capacity(n);
        for _ in 0..n {
            if slice.remaining() < 2 {
                return Err(LogError::Corruption("checkpoint truncated".to_string()));
            }
            let cid_len = slice.get_u16() as usize;
            if slice.remaining() < cid_len + 2 {
                return Err(LogError::Corruption("checkpoint truncated".to_string()));
            }
            let cid_bytes = slice.copy_to_bytes(cid_len);
            let cid = String::from_utf8(cid_bytes.to_vec())
                .map_err(|_| LogError::Corruption("cid not utf8".to_string()))?;
            let topic_len = slice.get_u16() as usize;
            if slice.remaining() < topic_len + 8 {
                return Err(LogError::Corruption("checkpoint truncated".to_string()));
            }
            let topic_bytes = slice.copy_to_bytes(topic_len);
            let topic = String::from_utf8(topic_bytes.to_vec())
                .map_err(|_| LogError::Corruption("topic not utf8".to_string()))?;
            let wal_id = slice.get_u64();
            cursors.insert((cid, topic), wal_id);
        }
        Ok(Self {
            snapshot_id,
            cursors,
        })
    }
}

/// Broker-level entry stored in the WAL. Messages are durable
/// publications; Acks record that a particular (client, topic) consumer
/// has acknowledged delivery of `acked_wal_id`. Ack records let
/// `replay_from_wal` skip messages already delivered before a crash.
#[derive(Debug, Clone)]
enum WalEntry {
    Message(WalMessageRecord),
    Ack(WalAckRecord),
}

impl WalEntry {
    fn encode(&self) -> Result<Bytes, LogError> {
        match self {
            WalEntry::Message(m) => m.encode_with_kind(),
            WalEntry::Ack(a) => a.encode_with_kind(),
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, LogError> {
        if bytes.is_empty() {
            return Err(LogError::Corruption("empty WAL entry".to_string()));
        }
        match bytes[0] {
            WAL_KIND_MESSAGE => Ok(WalEntry::Message(WalMessageRecord::decode_after_kind(
                &bytes[1..],
            )?)),
            WAL_KIND_ACK => Ok(WalEntry::Ack(WalAckRecord::decode_after_kind(&bytes[1..])?)),
            other => Err(LogError::Corruption(format!(
                "unknown WAL entry kind {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
struct WalMessageRecord {
    topic: String,
    qos: QoSLevel,
    payload: Bytes,
}

#[derive(Debug, Clone)]
struct WalAckRecord {
    client_id: String,
    topic: String,
    acked_wal_id: u64,
}

impl WalAckRecord {
    fn encode_with_kind(&self) -> Result<Bytes, LogError> {
        let cid_bytes = self.client_id.as_bytes();
        let topic_bytes = self.topic.as_bytes();
        let cid_len = u16::try_from(cid_bytes.len())
            .map_err(|_| LogError::Corruption("client_id too long for WAL ack".to_string()))?;
        let topic_len = u16::try_from(topic_bytes.len())
            .map_err(|_| LogError::Corruption("topic too long for WAL ack".to_string()))?;

        let mut buf = BytesMut::with_capacity(1 + 2 + cid_bytes.len() + 2 + topic_bytes.len() + 8);
        buf.put_u8(WAL_KIND_ACK);
        buf.put_u16(cid_len);
        buf.put_slice(cid_bytes);
        buf.put_u16(topic_len);
        buf.put_slice(topic_bytes);
        buf.put_u64(self.acked_wal_id);
        Ok(buf.freeze())
    }

    fn decode_after_kind(bytes: &[u8]) -> Result<Self, LogError> {
        if bytes.len() < 2 {
            return Err(LogError::Corruption("ack record too short".to_string()));
        }
        let mut slice = bytes;
        let cid_len = slice.get_u16() as usize;
        if slice.remaining() < cid_len + 2 {
            return Err(LogError::Corruption("ack: cid len overflow".to_string()));
        }
        let cid_bytes = slice.copy_to_bytes(cid_len);
        let client_id = String::from_utf8(cid_bytes.to_vec())
            .map_err(|_| LogError::Corruption("ack: invalid client_id utf8".to_string()))?;

        let topic_len = slice.get_u16() as usize;
        if slice.remaining() < topic_len + 8 {
            return Err(LogError::Corruption("ack: topic len overflow".to_string()));
        }
        let topic_bytes = slice.copy_to_bytes(topic_len);
        let topic = String::from_utf8(topic_bytes.to_vec())
            .map_err(|_| LogError::Corruption("ack: invalid topic utf8".to_string()))?;

        let acked_wal_id = slice.get_u64();
        Ok(Self {
            client_id,
            topic,
            acked_wal_id,
        })
    }
}

impl WalMessageRecord {
    fn encode_with_kind(&self) -> Result<Bytes, LogError> {
        let mut buf = BytesMut::new();
        buf.put_u8(WAL_KIND_MESSAGE);

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

    /// Decode the payload of a `WAL_KIND_MESSAGE` entry. The kind byte has
    /// already been consumed by the caller.
    fn decode_after_kind(bytes: &[u8]) -> Result<Self, LogError> {
        if bytes.len() < 3 {
            return Err(LogError::Corruption(
                "WAL message record too short".to_string(),
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

    /// Returns `Some(wal_id)` if the tag matched an inflight QoS1 entry
    /// (caller can then journal the ack), or `None` if the tag was unknown.
    /// `wal_id` is `None` inside the Some when the message was published
    /// non-durably (no WAL backing).
    fn ack(&self, tag: DeliveryTag) -> Option<Option<u64>> {
        let mut inner = self.inner.lock();
        inner.inflight.remove(&tag).map(|entry| entry.wal_id)
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

    fn maintenance_tick(
        &self,
        now: std::time::Instant,
        max_retries: u32,
        _base_delay: Duration,
    ) -> SmallVec<[(Bytes, DropReason); 4]> {
        let mut inner = self.inner.lock();
        let mut dropped: SmallVec<[(Bytes, DropReason); 4]> = SmallVec::new();

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
                    if let Some(removed) = inner.pending_entries.remove(&expiration.tag) {
                        dropped.push((removed.payload, DropReason::TtlExpired));
                    }
                }
                continue;
            }

            if let Some(entry) = inner.inflight.get(&expiration.tag) {
                if entry
                    .ttl
                    .map(|ttl| entry.created_at + ttl == expiration.expires_at)
                    .unwrap_or(false)
                {
                    if let Some(removed) = inner.inflight.remove(&expiration.tag) {
                        dropped.push((removed.payload, DropReason::TtlExpired));
                    }
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
                if let Some(removed) = inner.inflight.remove(&retry.tag) {
                    dropped.push((removed.payload, DropReason::MaxRetriesExceeded));
                }
                continue;
            }

            if let Some(mut entry) = inner.inflight.remove(&retry.tag) {
                entry.next_delivery_at = now;
                inner.pending.push_back(entry.tag);
                inner.pending_entries.insert(entry.tag, entry);
            }
        }

        dropped
    }
}

/// Why a queue entry was removed by maintenance. Surfaced to the broker
/// so it can publish a synthetic message to the dead-letter topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    TtlExpired,
    MaxRetriesExceeded,
}

impl Topic {
    fn new(name: TopicName) -> Self {
        Self {
            name,
            subscribers: RwLock::new(HashMap::new()),
            consumer_groups: RwLock::new(HashMap::new()),
            published_total: ShardedCounter::new(),
            delivered_total: ShardedCounter::new(),
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
            ready: std::sync::atomic::AtomicBool::new(false),
            messages_published_total: ShardedCounter::new(),
            messages_delivered_total: ShardedCounter::new(),
            push_dropped_total: CachePadded::new(std::sync::atomic::AtomicU64::new(0)),
            max_wal_id_observed: CachePadded::new(AtomicU64::new(0)),
            ack_cursors: RwLock::new(HashMap::new()),
            publish_fanout_ns: Mutex::new(
                Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3)
                    .expect("histogram bounds valid"),
            ),
            publish_count: ShardedCounter::new(),
            wildcard_subs: RwLock::new(Vec::new()),
            wildcard_subs_len: AtomicU64::new(0),
            drain_notify: Notify::new(),
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
            ready: std::sync::atomic::AtomicBool::new(false),
            messages_published_total: ShardedCounter::new(),
            messages_delivered_total: ShardedCounter::new(),
            push_dropped_total: CachePadded::new(AtomicU64::new(0)),
            max_wal_id_observed: CachePadded::new(AtomicU64::new(0)),
            ack_cursors: RwLock::new(HashMap::new()),
            publish_fanout_ns: Mutex::new(
                Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3)
                    .expect("histogram bounds valid"),
            ),
            publish_count: ShardedCounter::new(),
            wildcard_subs: RwLock::new(Vec::new()),
            wildcard_subs_len: AtomicU64::new(0),
            drain_notify: Notify::new(),
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
        self.messages_published_total.load()
    }

    pub fn messages_delivered_total(&self) -> u64 {
        self.messages_delivered_total.load()
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
        self.subscribe_inner(client_id, topic, qos, None, None, None)
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
        self.subscribe_inner(
            client_id,
            topic,
            qos,
            Some(conn_id),
            Some(push_sender),
            None,
        )
    }

    /// Subscribe in shared-buffer push mode (v2 fast path). Messages are
    /// encoded directly into `slot.buf` by `encoder` and signaled via
    /// `slot.notify`. The connection's writer task drains the buffer with
    /// a single `write_all` per drain.
    pub fn subscribe_with_slot(
        &self,
        client_id: ClientId,
        topic: TopicName,
        qos: QoSLevel,
        conn_id: u64,
        slot: Arc<PushSlot>,
        encoder: DeliveryEncoder,
    ) -> SubscriptionId {
        self.subscribe_inner(
            client_id,
            topic,
            qos,
            Some(conn_id),
            None,
            Some((slot, encoder)),
        )
    }

    /// Subscribe to `topic` as a member of a named consumer group. Each
    /// publish to the topic is delivered to exactly one member of the
    /// group (round-robin), in addition to all non-grouped subscribers.
    /// Use this for queue-group / competing-consumers semantics
    /// (Kafka's consumer groups, NATS's queue groups).
    #[allow(clippy::too_many_arguments)]
    pub fn subscribe_with_slot_in_group(
        &self,
        client_id: ClientId,
        topic: TopicName,
        qos: QoSLevel,
        conn_id: u64,
        slot: Arc<PushSlot>,
        encoder: DeliveryEncoder,
        group: String,
    ) -> SubscriptionId {
        self.subscribe_inner_grouped(
            client_id,
            topic,
            qos,
            Some(conn_id),
            None,
            Some((slot, encoder)),
            Some(group),
        )
    }

    /// Subscribe with a NATS-style subject pattern (may contain `*` and
    /// `>` wildcards). Exact patterns route through the fast `TopicShards`
    /// path; wildcard patterns are added to a parallel list scanned on
    /// every publish.
    pub fn subscribe_pattern(
        &self,
        client_id: ClientId,
        pattern: SubjectPattern,
        qos: QoSLevel,
    ) -> SubscriptionId {
        if pattern.is_exact() {
            // Reconstruct the exact subject from segments.
            let exact = self.exact_subject_from_pattern(&pattern);
            return self.subscribe(client_id, exact, qos);
        }
        let sub_id = self.subscribe_inner(
            client_id,
            // Wildcard patterns aren't anchored to any one topic. We
            // store a synthetic TopicName so the unsubscribe path's
            // topic-removal logic stays uniform; the fanout path consults
            // wildcard_subs directly, not the per-topic list.
            TopicName::from_str(pattern.as_str()),
            qos,
            None,
            None,
            None,
        );
        {
            let mut wcs = self.wildcard_subs.write();
            wcs.push((pattern, sub_id));
            self.wildcard_subs_len
                .store(wcs.len() as u64, Ordering::Release);
        }
        sub_id
    }

    /// Subscribe with a wildcard pattern in shared-buffer push mode.
    /// Mirror of `subscribe_pattern` for v2 push subscribers.
    pub fn subscribe_pattern_with_slot(
        &self,
        client_id: ClientId,
        pattern: SubjectPattern,
        qos: QoSLevel,
        conn_id: u64,
        slot: Arc<PushSlot>,
        encoder: DeliveryEncoder,
    ) -> SubscriptionId {
        if pattern.is_exact() {
            let exact = self.exact_subject_from_pattern(&pattern);
            return self.subscribe_with_slot(client_id, exact, qos, conn_id, slot, encoder);
        }
        let sub_id = self.subscribe_inner(
            client_id,
            TopicName::from_str(pattern.as_str()),
            qos,
            Some(conn_id),
            None,
            Some((slot, encoder)),
        );
        {
            let mut wcs = self.wildcard_subs.write();
            wcs.push((pattern, sub_id));
            self.wildcard_subs_len
                .store(wcs.len() as u64, Ordering::Release);
        }
        sub_id
    }

    fn exact_subject_from_pattern(&self, pattern: &SubjectPattern) -> TopicName {
        // is_exact ⇒ all segments are Literal and there's no tail. Join
        // them back into a `.`-separated string.
        let mut parts: Vec<&str> = Vec::with_capacity(pattern.segments.len());
        for seg in &pattern.segments {
            if let PatternSegment::Literal(s) = seg {
                parts.push(s.as_ref());
            }
        }
        TopicName::from_str(&parts.join("."))
    }

    fn subscribe_inner(
        &self,
        client_id: ClientId,
        topic: TopicName,
        qos: QoSLevel,
        conn_id: Option<u64>,
        push_sender: Option<PushSender>,
        push_slot: Option<(Arc<PushSlot>, DeliveryEncoder)>,
    ) -> SubscriptionId {
        self.subscribe_inner_grouped(client_id, topic, qos, conn_id, push_sender, push_slot, None)
    }

    /// Internal subscribe path that accepts an optional consumer group.
    /// `group = None` is the original broadcast behavior (subscriber
    /// lands in `Topic.subscribers`). `group = Some(g)` routes the
    /// subscriber into `Topic.consumer_groups[g].members` instead, so
    /// the publish path will pick exactly one member per group per
    /// publish (round-robin).
    #[allow(clippy::too_many_arguments)]
    fn subscribe_inner_grouped(
        &self,
        client_id: ClientId,
        topic: TopicName,
        qos: QoSLevel,
        conn_id: Option<u64>,
        push_sender: Option<PushSender>,
        push_slot: Option<(Arc<PushSlot>, DeliveryEncoder)>,
        group: Option<String>,
    ) -> SubscriptionId {
        let queue = SubscriberQueue::new(qos, self.config.per_subscriber_queue_capacity);
        let subscriber = Arc::new(Subscriber {
            client_id,
            queue,
            push_sender,
            push_slot,
            push_next_tag: AtomicU64::new(1),
        });

        let topic_arc = self.topics.get_or_insert(topic.clone());

        let sub_id = SubscriptionId(self.next_subscription_id.fetch_add(1, Ordering::Relaxed));

        match &group {
            None => {
                let mut subs = topic_arc.subscribers.write();
                subs.insert(sub_id, subscriber.clone());
            }
            Some(g) => {
                // Group subscribers do NOT land in `Topic.subscribers`
                // — they only receive load-balanced deliveries via the
                // consumer-group path. Get-or-create the group entry.
                let cg = {
                    let mut groups = topic_arc.consumer_groups.write();
                    groups
                        .entry(g.clone())
                        .or_insert_with(|| Arc::new(ConsumerGroup::new()))
                        .clone()
                };
                let mut members = cg.members.write();
                members.push((sub_id, subscriber.clone()));
            }
        }

        {
            let mut shard = self.subscriptions.shard_for(sub_id).write();
            shard.insert(
                sub_id,
                SubscriptionRef {
                    topic,
                    subscriber,
                    group,
                },
            );
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

        // Then remove from either the topic's broadcast subscriber
        // map (non-grouped) or the matching consumer-group member
        // list (grouped).
        for (sid, sub_ref) in &removed {
            if let Some(topic) = self.topics.get(&sub_ref.topic) {
                match &sub_ref.group {
                    None => {
                        let mut topic_subs = topic.subscribers.write();
                        topic_subs.remove(sid);
                    }
                    Some(g) => {
                        let groups = topic.consumer_groups.read();
                        if let Some(cg) = groups.get(g) {
                            let mut members = cg.members.write();
                            members.retain(|(s, _)| s != sid);
                        }
                    }
                }
            }
        }
        // Drop any wildcard entries belonging to these subs.
        if !removed.is_empty() {
            let removed_ids: std::collections::HashSet<SubscriptionId> =
                removed.iter().map(|(sid, _)| *sid).collect();
            let mut wcs = self.wildcard_subs.write();
            wcs.retain(|(_, sid)| !removed_ids.contains(sid));
            self.wildcard_subs_len
                .store(wcs.len() as u64, Ordering::Release);
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

    /// Snapshot the publish-fanout latency histogram. Cheap: locks the
    /// histogram briefly, computes percentiles. Intended for periodic
    /// scraping by the metrics endpoint, not the hot path.
    pub fn publish_fanout_latency(&self) -> LatencyStats {
        let h = self.publish_fanout_ns.lock();
        LatencyStats {
            count: h.len(),
            sum_ns: h
                .iter_recorded()
                .map(|v| v.value_iterated_to() * v.count_at_value())
                .sum(),
            p50_ns: h.value_at_quantile(0.50),
            p95_ns: h.value_at_quantile(0.95),
            p99_ns: h.value_at_quantile(0.99),
            max_ns: h.max(),
        }
    }

    /// Unsubscribe a single subscription. Used by the slow-consumer
    /// `DropSubscription` policy. Removes the sub from the global shard
    /// AND from the topic's subscriber list AND from any tracking
    /// `connection_subs` entry. Returns true if the sub existed.
    fn unsubscribe_one(&self, sub_id: SubscriptionId) -> bool {
        let removed = {
            let mut shard = self.subscriptions.shard_for(sub_id).write();
            shard.remove(&sub_id)
        };
        let Some(sub_ref) = removed else {
            return false;
        };
        if let Some(topic) = self.topics.get(&sub_ref.topic) {
            match &sub_ref.group {
                None => {
                    let mut topic_subs = topic.subscribers.write();
                    topic_subs.remove(&sub_id);
                }
                Some(g) => {
                    let groups = topic.consumer_groups.read();
                    if let Some(cg) = groups.get(g) {
                        let mut members = cg.members.write();
                        members.retain(|(s, _)| *s != sub_id);
                    }
                }
            }
        }
        // Remove from the connection_subs reverse-map too. We don't know
        // which conn this sub belonged to; walk the (small) map.
        let mut conn_map = self.connection_subs.write();
        for (_conn, subs) in conn_map.iter_mut() {
            subs.retain(|s| *s != sub_id);
        }
        conn_map.retain(|_, subs| !subs.is_empty());
        // Drop the wildcard entry too (no-op if this was an exact-match
        // subscription).
        {
            let mut wcs = self.wildcard_subs.write();
            wcs.retain(|(_, sid)| *sid != sub_id);
            self.wildcard_subs_len
                .store(wcs.len() as u64, Ordering::Release);
        }
        true
    }

    /// Mark the broker as ready to accept traffic. Called by the daemon
    /// after WAL replay completes.
    pub fn mark_ready(&self) {
        self.ready.store(true, Ordering::SeqCst);
    }

    /// True iff the broker has finished startup (WAL replay) and is not
    /// shutting down. Used by the readiness probe.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst) && !self.is_shutting_down()
    }

    /// Snapshot of per-topic counters. The topic registry is sharded; this
    /// walks all shards under read locks. Cost is O(num_topics); fine for
    /// Prometheus's typical 15-second scrape interval.
    pub fn topic_metrics(&self) -> Vec<TopicMetrics> {
        let mut out = Vec::new();
        for shard in &self.topics.shards {
            let guard = shard.read();
            for topic in guard.values() {
                out.push(TopicMetrics {
                    topic: topic.name.clone(),
                    published_total: topic.published_total.load(),
                    delivered_total: topic.delivered_total.load(),
                    subscriber_count: topic.subscribers.read().len(),
                });
            }
        }
        out
    }

    #[inline(always)]
    pub fn publish(&self, topic: &TopicName, payload: Bytes, qos: QoSLevel) {
        self.publish_with_wal_id(topic, payload, qos, None, None);
    }

    /// Resolve a topic to a cacheable handle. Returns `None` if no
    /// `Topic` record exists yet (no exact-match subscribers have ever
    /// subscribed). Wildcard subscribers do not create a `Topic`
    /// record, so a publish to a topic with only wildcard subs will
    /// keep returning `None` here — that's fine; the
    /// [`Self::publish_resolved`] hot path falls back to the wildcard
    /// scan automatically.
    ///
    /// Callers should hold this handle across many publishes to skip
    /// the per-publish `TopicShards.get()` (RwLock read + HashMap
    /// lookup + `Arc::clone`). Topic records are never removed, so
    /// the cached handle is valid for the broker's lifetime.
    #[inline(always)]
    pub fn resolve_topic(&self, name: &TopicName) -> Option<ResolvedTopic> {
        self.topics.get(name).map(|inner| ResolvedTopic { inner })
    }

    /// Publish using a pre-resolved topic handle from
    /// [`Self::resolve_topic`]. Skips the topic registry lookup for
    /// the exact-match fanout. Wildcard fanout still runs.
    #[inline(always)]
    pub fn publish_resolved(
        &self,
        name: &TopicName,
        resolved: &ResolvedTopic,
        payload: Bytes,
        qos: QoSLevel,
    ) {
        self.publish_inner(name, Some(resolved.inner.clone()), payload, qos, None, None);
    }

    /// Publish with a pre-resolved topic handle and a per-message TTL
    /// override. Combines [`Self::publish_resolved`] with the
    /// [`Self::publish_with_ttl`] semantics.
    #[inline(always)]
    pub fn publish_resolved_with_ttl(
        &self,
        name: &TopicName,
        resolved: &ResolvedTopic,
        payload: Bytes,
        qos: QoSLevel,
        ttl: Option<Duration>,
    ) {
        self.publish_inner(name, Some(resolved.inner.clone()), payload, qos, None, ttl);
    }

    /// Publish with a per-message TTL override that applies only to this
    /// message's enqueue. `None` falls back to `BrokerConfig::message_ttl`.
    #[inline]
    pub fn publish_with_ttl(
        &self,
        topic: &TopicName,
        payload: Bytes,
        qos: QoSLevel,
        ttl: Option<Duration>,
    ) {
        self.publish_with_wal_id(topic, payload, qos, None, ttl);
    }

    // No `#[tracing::instrument]` on this — the macro still creates,
    // enters, and exits a span per call even when the level is filtered
    // out by the default subscriber, which costs us measurable time at
    // multi-M ops/sec. If a future debugging session needs trace-level
    // visibility into publish, add it back behind a `cfg(debug_assertions)`
    // gate or an explicit feature flag.
    #[inline(always)]
    fn publish_with_wal_id(
        &self,
        topic_name: &TopicName,
        payload: Bytes,
        qos: QoSLevel,
        wal_id: Option<u64>,
        ttl_override: Option<Duration>,
    ) {
        self.publish_inner(topic_name, None, payload, qos, wal_id, ttl_override);
    }

    /// Inner publish path. `pre_resolved` is the topic handle from a
    /// prior [`Self::resolve_topic`] call when the caller has cached
    /// it; `None` falls back to the per-publish `TopicShards.get()`.
    #[inline(always)]
    fn publish_inner(
        &self,
        topic_name: &TopicName,
        pre_resolved: Option<Arc<Topic>>,
        payload: Bytes,
        qos: QoSLevel,
        wal_id: Option<u64>,
        ttl_override: Option<Duration>,
    ) {
        if self.is_shutting_down() {
            return;
        }

        // Decide up-front whether this publish gets a latency sample.
        // The non-sampled path skips both `Instant::now()` and the
        // histogram lock, leaving only one Relaxed atomic add on the
        // hot path. Sample interval is 1-in-64.
        let n = self.publish_count.add(1).wrapping_add(1);
        let sample_start: Option<std::time::Instant> = if n & PUBLISH_LATENCY_SAMPLE_MASK == 0 {
            Some(std::time::Instant::now())
        } else {
            None
        };

        // Resolve the effective TTL for this publish: per-message override
        // when set, otherwise the broker default. The Option-Some wrapping
        // is preserved so SubscriberQueue::enqueue can still distinguish
        // "no TTL at all" from "explicit TTL of 0", though we don't expose
        // the latter to clients yet.
        let effective_ttl = ttl_override.or(Some(self.config.message_ttl));

        // Track the highest WAL id we've seen so a future
        // `current_snapshot()` reflects it. Cheap: relaxed atomic
        // compare-and-swap loop, only entered for WAL-backed publishes.
        if let Some(id) = wal_id {
            let mut cur = self.max_wal_id_observed.load(Ordering::Relaxed);
            while id > cur {
                match self.max_wal_id_observed.compare_exchange_weak(
                    cur,
                    id,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(observed) => cur = observed,
                }
            }
        }

        // We always count the publish (even if no subscribers match),
        // since publishers shouldn't have to know which topics have
        // active subscriptions.
        self.messages_published_total.add(1);

        // Two-source fanout snapshot:
        //  (a) per-topic exact-match subscribers from `TopicShards`.
        //  (b) wildcard-pattern subscribers whose pattern matches the
        //      publish subject. We always run (b) even if no Topic
        //      exists for this subject, because a wildcard sub may be
        //      the only consumer.
        // Skip the `TopicShards.get()` (RwLock read + HashMap +
        // Arc::clone) when the caller already cached the resolved
        // handle from a prior `resolve_topic`. Falls back to the
        // registry lookup when no cache is available.
        let topic_opt = pre_resolved.or_else(|| self.topics.get(topic_name));
        let mut snapshot: SubscriberSnapshot<'_> = SmallVec::new();
        if let Some(topic) = &topic_opt {
            topic.published_total.add(1);
            let subscribers = topic.subscribers.read();
            for (id, sub) in subscribers.iter() {
                snapshot.push((*id, sub.clone()));
            }
            drop(subscribers);

            // Consumer-group fanout: for each registered group on this
            // topic, pick exactly one member round-robin and add it to
            // the snapshot. This is what gives us NATS-queue-group /
            // Kafka-consumer-group semantics: the *group* sees one
            // delivery per publish, regardless of how many members it
            // has. Non-grouped subscribers above still see every
            // publish.
            //
            // Snapshot the groups first (clone the Arc<ConsumerGroup>
            // pointers), then drop the topic-level lock before walking
            // — keeps the topic's lock window tight even when many
            // groups are registered.
            let groups_snapshot: SmallVec<[Arc<ConsumerGroup>; 4]> = {
                let groups = topic.consumer_groups.read();
                if groups.is_empty() {
                    SmallVec::new()
                } else {
                    groups.values().cloned().collect()
                }
            };
            for cg in &groups_snapshot {
                let members = cg.members.read();
                if members.is_empty() {
                    continue;
                }
                let idx = cg.next_idx.fetch_add(1, Ordering::Relaxed) % members.len();
                let (sid, sub) = &members[idx];
                snapshot.push((*sid, sub.clone()));
            }
        }
        // Wildcard fanout: walk the pattern list, match each pattern
        // against the publish subject, and pull the matching subscriber
        // Arcs out of the sharded subscriptions map. Cost is O(W) where
        // W is the number of wildcard subscriptions; for hundreds of
        // patterns this is well under a microsecond.
        //
        // The atomic-counter early-exit avoids the parking_lot read
        // lock acquisition entirely when no wildcard subs are
        // registered (the common case in practice).
        if self.wildcard_subs_len.load(Ordering::Acquire) > 0 {
            let wcs = self.wildcard_subs.read();
            for (pattern, sid) in wcs.iter() {
                if pattern.matches(topic_name.as_str()) {
                    let shard = self.subscriptions.shard_for(*sid).read();
                    if let Some(sub_ref) = shard.get(sid) {
                        snapshot.push((*sid, sub_ref.subscriber.clone()));
                    }
                }
            }
        }
        // The rest of the function operates on `snapshot`. The
        // per-topic delivered_total counter will be updated only if a
        // Topic exists; wildcard deliveries don't bump per-topic stats
        // for a topic that has no exact-match subscribers (no Topic
        // record exists to count against).
        let topic_for_metrics = topic_opt.as_ref();

        let mut subs_to_drop: SmallVec<[SubscriptionId; 4]> = SmallVec::new();
        // Accumulate successful-delivery counts locally so we issue one
        // fetch_add per counter at the end of the fanout instead of N.
        // For a 64-sub fanout this drops 128 atomic ops to 2.
        let mut delivered: u64 = 0;
        for (sub_id, subscriber) in snapshot.iter() {
            if let Some((slot, encoder)) = &subscriber.push_slot {
                // Shared-buffer push (v2 fast path): encode the DELIVER
                // frame directly into the conn's shared BytesMut and ping
                // the writer. No channel hop, no separate writer-side
                // encode step.
                let tag = subscriber.push_next_tag.fetch_add(1, Ordering::Relaxed);
                if qos == QoSLevel::AtLeastOnce {
                    subscriber.queue.register_push_inflight(
                        DeliveryTag(tag),
                        payload.clone(),
                        wal_id,
                        effective_ttl,
                    );
                }
                let wire_tag = if qos == QoSLevel::AtLeastOnce { tag } else { 0 };
                let mut over_threshold = false;
                {
                    let mut buf = slot.buf.lock();
                    if buf.len() >= self.config.slow_consumer_buffer_bytes {
                        over_threshold = true;
                    } else {
                        encoder(&mut buf, qos, wire_tag, topic_name.as_str(), &payload);
                    }
                }
                if over_threshold {
                    self.push_dropped_total.fetch_add(1, Ordering::Relaxed);
                    if qos == QoSLevel::AtLeastOnce {
                        subscriber.queue.cancel_push_inflight(DeliveryTag(tag));
                    }
                    if matches!(
                        self.config.slow_consumer_policy,
                        SlowConsumerPolicy::DropSubscription,
                    ) {
                        subs_to_drop.push(*sub_id);
                    }
                } else {
                    slot.notify.notify_one();
                    delivered = delivered.saturating_add(1);
                }
            } else if let Some(sender) = &subscriber.push_sender {
                // Legacy channel-based push.
                let tag = subscriber.push_next_tag.fetch_add(1, Ordering::Relaxed);

                if qos == QoSLevel::AtLeastOnce {
                    subscriber.queue.register_push_inflight(
                        DeliveryTag(tag),
                        payload.clone(),
                        wal_id,
                        effective_ttl,
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
                        subscriber.queue.cancel_push_inflight(DeliveryTag(tag));
                    }
                    if matches!(
                        self.config.slow_consumer_policy,
                        SlowConsumerPolicy::DropSubscription,
                    ) {
                        subs_to_drop.push(*sub_id);
                    }
                } else {
                    delivered = delivered.saturating_add(1);
                }
            } else {
                // Poll path (v1): unchanged.
                subscriber
                    .queue
                    .enqueue(payload.clone(), qos, wal_id, effective_ttl);
                delivered = delivered.saturating_add(1);
            }
        }

        // Single batched atomic add per counter (vs one-per-delivery
        // before). Skipped entirely when nothing was delivered.
        if delivered > 0 {
            if let Some(t) = topic_for_metrics {
                t.delivered_total.add(delivered);
            }
            self.messages_delivered_total.add(delivered);
        }

        // Apply the DropSubscription slow-consumer policy after the fanout
        // loop so we don't hold any per-topic locks across the unsubscribe
        // path. `unsubscribe_one` takes write locks on the subscriptions
        // shard and on the topic's subscribers map.
        for sid in subs_to_drop {
            self.unsubscribe_one(sid);
        }

        // Record latency only on sampled publishes. Non-sampled
        // publishes skipped the `Instant::now()` capture too, so the
        // hot path's only cost was one Relaxed atomic add at function
        // entry.
        if let Some(start) = sample_start {
            let elapsed_ns = start.elapsed().as_nanos() as u64;
            if let Some(mut h) = self.publish_fanout_ns.try_lock() {
                h.saturating_record(elapsed_ns);
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

        let entry = WalEntry::Message(WalMessageRecord {
            topic: topic.as_str().to_string(),
            qos,
            payload: payload.clone(),
        });

        let encoded = entry.encode()?;
        // append_durable returns only after fsync covers this record. This
        // is what makes "publish_durable returned Ok" mean "on disk".
        let wal_id = wal.append_durable(encoded).await?;

        self.publish_with_wal_id(topic, payload, qos, Some(wal_id), None);

        Ok(wal_id)
    }

    /// Replay all records currently present in the WAL and enqueue them to
    /// the subscriptions present on this broker, **skipping any message
    /// already covered by an Ack record for the same `(client_id, topic)`
    /// pair**. The two-pass design lets a restarted broker re-attach
    /// previously-known consumers (by `client_id`) and recover only the
    /// undelivered tail of the log instead of replaying everything.
    pub async fn replay_from_wal(&self) -> Result<(), LogError> {
        self.replay_from_wal_with_checkpoint(None).await
    }

    /// Like `replay_from_wal`, but seeded with a previously-saved
    /// [`CheckpointSnapshot`]. Replay still walks the WAL (so any Ack or
    /// Message records past `snapshot_id` are honored), but Message
    /// records with id <= `snapshot_id` are skipped wholesale because
    /// they're already represented by the checkpoint's cursors.
    pub async fn replay_from_wal_with_checkpoint(
        &self,
        checkpoint: Option<CheckpointSnapshot>,
    ) -> Result<(), LogError> {
        let wal = match &self.wal {
            Some(w) => w.clone(),
            None => return Ok(()),
        };

        // Start from the checkpoint's snapshot_id + 1 if we have one;
        // otherwise from 1. Any earlier records were already accounted
        // for in the snapshot's cursors.
        let (mut ack_cursors, start_from) = match checkpoint {
            Some(cp) => (cp.cursors, cp.snapshot_id.saturating_add(1)),
            None => (HashMap::<(String, String), u64>::new(), 1),
        };

        let records: Vec<WalRecord> = wal.iterate_from(start_from).await?;

        // Pass 1: refine per-(client_id, topic) cursors using any Ack
        // records past the checkpoint.
        for record in &records {
            if let Ok(WalEntry::Ack(ack)) = WalEntry::decode(&record.payload) {
                let key = (ack.client_id, ack.topic);
                let entry = ack_cursors.entry(key).or_insert(0);
                if ack.acked_wal_id > *entry {
                    *entry = ack.acked_wal_id;
                }
            }
        }

        // Pass 2: re-enqueue Message records to subscribers, filtering by
        // ack cursors. We bypass the normal fanout (publish_with_wal_id)
        // because that doesn't know about per-consumer cursors; instead
        // we walk the topic's subscriber snapshot directly so we can
        // consult `client_id` per subscriber.
        for record in records {
            let entry = match WalEntry::decode(&record.payload) {
                Ok(e) => e,
                Err(_) => continue, // already-rotten records are skipped, not fatal
            };
            let msg = match entry {
                WalEntry::Message(m) => m,
                WalEntry::Ack(_) => continue,
            };

            let topic_name = TopicName::new(msg.topic.clone());
            let topic = match self.topics.get(&topic_name) {
                Some(t) => t,
                None => continue, // no subscribers for this topic on this broker
            };

            let snapshot: SubscriberSnapshot<'_> = {
                let subs = topic.subscribers.read();
                subs.iter().map(|(id, s)| (*id, s.clone())).collect()
            };

            for (_sub_id, subscriber) in snapshot.iter() {
                let cursor_key = (subscriber.client_id.as_str().to_string(), msg.topic.clone());
                let acked_up_to = ack_cursors.get(&cursor_key).copied().unwrap_or(0);
                if record.id <= acked_up_to {
                    // Already delivered + acked before the crash.
                    continue;
                }
                // Re-deliver: enqueue exactly as a fresh publish would,
                // preserving wal_id so a future ack journals correctly.
                let payload = msg.payload.clone();
                let qos = msg.qos;
                let ttl = Some(self.config.message_ttl);
                if let Some((slot, encoder)) = &subscriber.push_slot {
                    let tag = subscriber.push_next_tag.fetch_add(1, Ordering::Relaxed);
                    if qos == QoSLevel::AtLeastOnce {
                        subscriber.queue.register_push_inflight(
                            DeliveryTag(tag),
                            payload.clone(),
                            Some(record.id),
                            ttl,
                        );
                    }
                    let wire_tag = if qos == QoSLevel::AtLeastOnce { tag } else { 0 };
                    {
                        let mut buf = slot.buf.lock();
                        encoder(&mut buf, qos, wire_tag, topic_name.as_str(), &payload);
                    }
                    slot.notify.notify_one();
                } else {
                    subscriber.queue.enqueue(payload, qos, Some(record.id), ttl);
                }
            }
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
        // Look up the inflight entry; capture client_id/topic/wal_id so we
        // can journal the ack after dropping the shard read lock.
        let (client_id, topic, wal_id_opt) = {
            let shard = self.subscriptions.shard_for(sub_id).read();
            let sub_ref = match shard.get(&sub_id) {
                Some(r) => r,
                None => return false,
            };
            let wal_id_opt = match sub_ref.subscriber.queue.ack(tag) {
                Some(wid) => wid,
                None => return false,
            };
            (
                sub_ref.subscriber.client_id.as_str().to_string(),
                sub_ref.topic.as_str().to_string(),
                wal_id_opt,
            )
        };

        // Update the in-memory cursor so a subsequent `current_snapshot()`
        // call reflects this ack without needing to walk the WAL. This is
        // cheap: one write-locked HashMap upsert.
        if let Some(wal_id) = wal_id_opt {
            let mut cursors = self.ack_cursors.write();
            let key = (client_id.clone(), topic.clone());
            let entry = cursors.entry(key).or_insert(0);
            if wal_id > *entry {
                *entry = wal_id;
            }
        }

        // Inflight count just dropped (we removed an entry). Wake any
        // graceful-shutdown waiter that's blocked on a drain.
        self.drain_notify.notify_waiters();

        // Journal the ack if (a) the message was WAL-backed and (b) the
        // broker is configured with a WAL. Channel-gated (no fsync wait):
        // ack records are idempotent, so losing the trailing ack on a
        // crash just means the message gets redelivered, which the client
        // is already prepared to handle.
        if let (Some(wal_id), Some(wal)) = (wal_id_opt, &self.wal) {
            let entry = WalEntry::Ack(WalAckRecord {
                client_id,
                topic,
                acked_wal_id: wal_id,
            });
            if let Ok(encoded) = entry.encode() {
                // Best-effort: backpressure / writer-stopped errors during
                // ack-journaling are non-fatal here; the worst case is a
                // redelivery on replay.
                let wal = wal.clone();
                tokio::spawn(async move {
                    let _ = wal.append(encoded).await;
                });
            }
        }
        true
    }

    /// Capture a [`CheckpointSnapshot`] of the broker's current recovery
    /// state. `snapshot_id` is the **smallest** acked-wal-id across all
    /// known consumers (or 0 if no consumer has acked anything yet) — i.e.
    /// "everything strictly below this is fully delivered". On replay,
    /// records with id <= `snapshot_id` are already fully accounted for
    /// in `cursors` and can be skipped; records past it are walked.
    pub fn current_snapshot(&self) -> CheckpointSnapshot {
        let cursors = self.ack_cursors.read().clone();
        let snapshot_id = cursors.values().copied().min().unwrap_or(0);
        CheckpointSnapshot {
            snapshot_id,
            cursors,
        }
    }

    /// Atomically write the current checkpoint snapshot to `path`. Writes
    /// to `path.tmp` then renames; partial writes can never overwrite a
    /// good checkpoint. The caller chooses where the file lives (typically
    /// inside the WAL directory).
    pub async fn write_checkpoint_to(&self, path: &Path) -> Result<(), LogError> {
        let snap = self.current_snapshot();
        let bytes = snap.encode()?;
        let tmp = path.with_extension("snap.tmp");
        // Make sure the parent directory exists.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(LogError::Io)?;
            }
        }
        tokio::fs::write(&tmp, &bytes[..])
            .await
            .map_err(LogError::Io)?;
        // tokio::fs::rename is atomic on the same filesystem.
        tokio::fs::rename(&tmp, path).await.map_err(LogError::Io)?;
        Ok(())
    }

    /// Try to load a [`CheckpointSnapshot`] from `path`. Returns `Ok(None)`
    /// if the file doesn't exist (fresh deployment); `Err(_)` only if it
    /// exists but is structurally bad (caller should fall back to a full
    /// replay).
    pub async fn load_checkpoint_from(path: &Path) -> Result<Option<CheckpointSnapshot>, LogError> {
        match tokio::fs::read(path).await {
            Ok(bytes) => Ok(Some(CheckpointSnapshot::decode(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(LogError::Io(e)),
        }
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
        // Collect (dlq_topic, payload) tuples while shard locks are held;
        // publish them after the walk so we don't reentrantly take the
        // same locks. Most maintenance ticks produce zero drops, so the
        // SmallVec inline storage handles the common case.
        let mut to_dlq: SmallVec<[(TopicName, Bytes); 4]> = SmallVec::new();
        let dlq_suffix = self.config.dlq_suffix.clone();

        for shard in &self.subscriptions.shards {
            for sub_ref in shard.read().values() {
                let dropped = sub_ref.subscriber.queue.maintenance_tick(
                    now,
                    self.config.max_retries,
                    self.config.retry_base_delay,
                );
                if let Some(suffix) = &dlq_suffix {
                    let topic_str = sub_ref.topic.as_str();
                    // Skip self-DLQ to avoid recursion: if the subscriber's
                    // own topic already ends in the DLQ suffix, dropped
                    // messages are silently lost rather than echoed back to
                    // the same DLQ.
                    if !topic_str.ends_with(suffix.as_str()) {
                        let dlq_topic = TopicName::from_str(&format!("{}{}", topic_str, suffix));
                        for (payload, _reason) in dropped {
                            to_dlq.push((dlq_topic.clone(), payload));
                        }
                    }
                }
            }
        }

        // Publish DLQ messages outside the shard read locks.
        for (topic, payload) in to_dlq {
            self.publish(&topic, payload, QoSLevel::AtMostOnce);
        }

        // Maintenance may have expired or dropped inflight entries; wake
        // any graceful-shutdown waiter so it re-checks immediately.
        self.drain_notify.notify_waiters();
    }

    /// Wait for `inflight_message_count()` to reach zero, or for `timeout`
    /// to elapse. Returns true if the queue drained within the budget,
    /// false on timeout. Used by graceful shutdown to avoid a fixed-cost
    /// polling loop: the broker fires `drain_notify` on every ack and
    /// every maintenance tick, so the wait wakes immediately when the
    /// last in-flight clears.
    pub async fn wait_for_drain(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.inflight_message_count() == 0 {
                return true;
            }
            // Subscribe to notifications BEFORE checking remaining-time so
            // a notification that fires between the inflight read and the
            // sleep is not lost (Notify holds a permit if a waiter races).
            let notified = self.drain_notify.notified();
            let remaining = match deadline.checked_duration_since(std::time::Instant::now()) {
                Some(d) if !d.is_zero() => d,
                _ => return self.inflight_message_count() == 0,
            };
            tokio::select! {
                _ = notified => continue,
                _ = tokio::time::sleep(remaining) => {
                    return self.inflight_message_count() == 0;
                }
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
            slow_consumer_policy: SlowConsumerPolicy::DropNewest,
            slow_consumer_buffer_bytes: 16 * 1024 * 1024,
            dlq_suffix: Some(".dlq".to_string()),
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
            slow_consumer_policy: SlowConsumerPolicy::DropNewest,
            slow_consumer_buffer_bytes: 16 * 1024 * 1024,
            dlq_suffix: Some(".dlq".to_string()),
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

    /// When a QoS1 message expires while inflight (TTL passed without
    /// ACK), it must be re-published to the DLQ topic
    /// `<topic><dlq_suffix>` if the broker has DLQ configured. A
    /// subscriber on the DLQ topic should receive the original payload.
    #[test]
    fn ttl_expired_message_goes_to_dlq() {
        let broker = Broker::new(BrokerConfig {
            default_qos: QoSLevel::AtLeastOnce,
            message_ttl: Duration::from_millis(50),
            per_subscriber_queue_capacity: 16,
            max_retries: 3,
            retry_base_delay: Duration::from_millis(10),
            slow_consumer_policy: SlowConsumerPolicy::DropNewest,
            slow_consumer_buffer_bytes: 16 * 1024 * 1024,
            dlq_suffix: Some(".dlq".to_string()),
        });

        let topic = TopicName::new("dlq-source");
        let dlq_topic = TopicName::new("dlq-source.dlq");

        let primary_sub = broker.subscribe(
            ClientId::new("primary"),
            topic.clone(),
            QoSLevel::AtLeastOnce,
        );
        let dlq_sub = broker.subscribe(
            ClientId::new("dlq-watcher"),
            dlq_topic.clone(),
            QoSLevel::AtMostOnce,
        );

        let payload = Bytes::from_static(b"will-expire");
        broker.publish(&topic, payload.clone(), QoSLevel::AtLeastOnce);

        // Pull the message into the inflight set on the primary sub but
        // never ack it; this simulates a consumer that died.
        let polled = broker.poll(primary_sub).expect("delivery");
        assert!(polled.delivery_tag.is_some());

        // Wait past TTL, then run maintenance.
        std::thread::sleep(Duration::from_millis(70));
        broker.maintenance_tick(Instant::now());

        // Primary sub must NOT see a re-delivery (the message expired).
        assert!(broker.poll(primary_sub).is_none());

        // DLQ subscriber should now have the original payload.
        let dlq_msg = broker.poll(dlq_sub).expect("DLQ delivery");
        assert_eq!(dlq_msg.payload, payload);
    }

    #[test]
    fn qos1_retry_after_timeout() {
        let broker = Broker::new(BrokerConfig {
            default_qos: QoSLevel::AtLeastOnce,
            message_ttl: Duration::from_secs(5),
            per_subscriber_queue_capacity: 16,
            max_retries: 3,
            retry_base_delay: Duration::from_millis(20),
            slow_consumer_policy: SlowConsumerPolicy::DropNewest,
            slow_consumer_buffer_bytes: 16 * 1024 * 1024,
            dlq_suffix: Some(".dlq".to_string()),
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
            slow_consumer_policy: SlowConsumerPolicy::DropNewest,
            slow_consumer_buffer_bytes: 16 * 1024 * 1024,
            dlq_suffix: Some(".dlq".to_string()),
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
        assert!(
            broker.poll(sub_id).is_none(),
            "push subscriber must not have a pending poll-path entry"
        );
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
        assert!(
            rx.recv_async().await.is_err(),
            "channel should close once last sender is dropped"
        );
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

    /// Phase 4: replay must skip already-acked messages on restart. Publish
    /// 5 messages durably, ack 4 of them, drop the broker, restart with
    /// the same WAL, re-subscribe with the *same client_id*, replay --
    /// only the unacked tail (1 message) should be re-delivered.
    #[tokio::test]
    async fn ack_journal_skips_already_acked_on_replay() {
        let mut path = std::env::temp_dir();
        path.push("core_ack_journal_replay");
        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::remove_file(&path);

        let wal = Arc::new(WriteAheadLog::open(&path).await.expect("open WAL"));

        let config = BrokerConfig {
            default_qos: QoSLevel::AtLeastOnce,
            message_ttl: Duration::from_secs(60),
            per_subscriber_queue_capacity: 32,
            max_retries: 3,
            retry_base_delay: Duration::from_millis(50),
            slow_consumer_policy: SlowConsumerPolicy::DropNewest,
            slow_consumer_buffer_bytes: 16 * 1024 * 1024,
            dlq_suffix: Some(".dlq".to_string()),
        };

        let topic = TopicName::new("ack-journal-topic");
        let client_id = ClientId::new("client-A");

        // -- pre-crash session --
        let acked_tags = {
            let broker = Broker::new_with_wal(config.clone(), wal.clone());
            let sub_id = broker.subscribe(client_id.clone(), topic.clone(), QoSLevel::AtLeastOnce);

            let mut tags = Vec::new();
            for i in 0..5u8 {
                let payload = Bytes::from(vec![i; 8]);
                broker
                    .publish_durable(&topic, payload, QoSLevel::AtLeastOnce)
                    .await
                    .unwrap();
                let polled = broker.poll(sub_id).expect("expected delivery");
                tags.push(polled.delivery_tag.expect("QoS1 has tag"));
            }
            // Ack the first 4 of 5; the last stays inflight at "crash".
            for tag in &tags[..4] {
                assert!(broker.ack(sub_id, *tag));
            }
            // Give the spawned ack-journal write tasks a moment to flush
            // through the WAL channel.
            tokio::time::sleep(Duration::from_millis(50)).await;
            wal.flush().await.unwrap();
            tags
        };

        // -- post-crash session: same WAL, same client_id, fresh broker --
        let broker2 = Broker::new_with_wal(config, wal.clone());
        let sub_id2 = broker2.subscribe(client_id, topic.clone(), QoSLevel::AtLeastOnce);
        broker2.replay_from_wal().await.unwrap();

        // Drain whatever the replay enqueued. Should be exactly the 1
        // unacked message (acked_tags[4] was never acked).
        let mut redelivered = 0;
        while let Some(_msg) = broker2.poll(sub_id2) {
            redelivered += 1;
        }
        assert_eq!(
            redelivered, 1,
            "expected exactly 1 unacked message to be re-delivered, got {redelivered}",
        );

        let _ = acked_tags; // silence unused
    }

    #[test]
    fn subject_pattern_parses_and_matches() {
        let exact = SubjectPattern::parse("orders.us.created").unwrap();
        assert!(exact.is_exact());
        assert!(exact.matches("orders.us.created"));
        assert!(!exact.matches("orders.us.shipped"));

        let single = SubjectPattern::parse("orders.us.*").unwrap();
        assert!(!single.is_exact());
        assert!(single.matches("orders.us.created"));
        assert!(single.matches("orders.us.shipped"));
        assert!(!single.matches("orders.us.created.priority")); // too many tokens
        assert!(!single.matches("orders.eu.created")); // literal mismatch

        let leading = SubjectPattern::parse("*.us.created").unwrap();
        assert!(leading.matches("orders.us.created"));
        assert!(leading.matches("users.us.created"));
        assert!(!leading.matches("us.created")); // too few tokens

        let tail = SubjectPattern::parse("orders.>").unwrap();
        assert!(tail.matches("orders.us.created"));
        assert!(tail.matches("orders.eu.shipped.priority"));
        assert!(!tail.matches("orders")); // tail requires >= 1 trailing token
        assert!(!tail.matches("users.us.created"));
    }

    #[test]
    fn subject_pattern_rejects_malformed() {
        assert!(SubjectPattern::parse("").is_err());
        assert!(SubjectPattern::parse("a..b").is_err());
        assert!(SubjectPattern::parse(".a").is_err());
        assert!(SubjectPattern::parse("a.").is_err());
        // `>` must be the last token.
        assert!(SubjectPattern::parse(">.a").is_err());
        assert!(SubjectPattern::parse("a.>.b").is_err());
    }

    #[tokio::test]
    async fn wildcard_subscription_receives_matching_publish() {
        let broker = test_broker();
        let pattern = SubjectPattern::parse("orders.*.created").unwrap();
        let (tx, rx) = flume::bounded::<DeliveryHandle>(64);

        let _sub = broker.subscribe_pattern(ClientId::new("c"), pattern, QoSLevel::AtMostOnce);
        // The above goes through the legacy poll path because we used the
        // non-slot variant; switch to slot-based pattern subscribe so we
        // can exercise the actual fanout receive.
        let _ = tx;
        let _ = rx;

        // Use the channel-based subscribe with a wildcard pattern via
        // the trait directly: use subscribe_pattern + push_sender wiring.
        // For a focused test, we'll subscribe via the channel API.
        let (tx2, rx2) = flume::bounded::<DeliveryHandle>(64);
        let _wc_sid = broker.subscribe_inner(
            ClientId::new("c2"),
            TopicName::from_str("orders.*.created"),
            QoSLevel::AtMostOnce,
            None,
            Some(tx2),
            None,
        );
        // Manually register the wildcard. Bumping wildcard_subs_len
        // mirrors what `subscribe_pattern` would do; without it the
        // atomic-gated fanout scan in publish_with_wal_id would skip
        // the new entry.
        let p = SubjectPattern::parse("orders.*.created").unwrap();
        {
            let mut wcs = broker.wildcard_subs.write();
            wcs.push((p, _wc_sid));
            broker
                .wildcard_subs_len
                .store(wcs.len() as u64, Ordering::Release);
        }

        // Publish to a matching subject.
        broker.publish(
            &TopicName::from_str("orders.us.created"),
            Bytes::from_static(b"x"),
            QoSLevel::AtMostOnce,
        );
        let handle = rx2.recv_async().await.expect("delivery");
        assert_eq!(handle.payload, Bytes::from_static(b"x"));

        // Publish to a NON-matching subject — no delivery on the wildcard.
        broker.publish(
            &TopicName::from_str("orders.us.shipped"),
            Bytes::from_static(b"y"),
            QoSLevel::AtMostOnce,
        );
        let res =
            tokio::time::timeout(std::time::Duration::from_millis(50), rx2.recv_async()).await;
        assert!(
            res.is_err(),
            "non-matching publish must not reach wildcard sub"
        );
    }

    #[test]
    fn checkpoint_snapshot_encode_decode_roundtrip() {
        let mut snap = CheckpointSnapshot::empty();
        snap.snapshot_id = 12345;
        snap.cursors
            .insert(("client-A".to_string(), "orders".to_string()), 10);
        snap.cursors
            .insert(("client-B".to_string(), "logs".to_string()), 42);

        let bytes = snap.encode().expect("encode");
        let decoded = CheckpointSnapshot::decode(&bytes).expect("decode");
        assert_eq!(decoded.snapshot_id, 12345);
        assert_eq!(decoded.cursors.len(), 2);
        assert_eq!(
            decoded
                .cursors
                .get(&("client-A".to_string(), "orders".to_string())),
            Some(&10),
        );
    }

    #[test]
    fn checkpoint_corruption_caught_by_crc() {
        let mut snap = CheckpointSnapshot::empty();
        snap.snapshot_id = 1;
        snap.cursors.insert(("c".to_string(), "t".to_string()), 7);
        let mut bytes = snap.encode().expect("encode").to_vec();
        // Flip a byte in the body.
        bytes[10] ^= 0xFF;
        let err = CheckpointSnapshot::decode(&bytes).unwrap_err();
        assert!(matches!(err, LogError::Corruption(_)));
    }

    /// Round-trip: write a checkpoint after acking some QoS1 messages,
    /// drop the broker, restart with the loaded checkpoint, and verify
    /// only unacked messages are re-delivered. Also verifies that the
    /// in-memory ack_cursors map is updated by Broker::ack so the
    /// snapshot doesn't need to re-walk the WAL.
    #[tokio::test]
    async fn replay_with_checkpoint_skips_acked_tail() {
        let mut wal_path = std::env::temp_dir();
        wal_path.push("core_replay_with_ckpt_wal");
        let _ = std::fs::remove_dir_all(&wal_path);

        let mut ckpt_path = std::env::temp_dir();
        ckpt_path.push("core_replay_with_ckpt.snap");
        let _ = std::fs::remove_file(&ckpt_path);

        let wal = Arc::new(WriteAheadLog::open(&wal_path).await.unwrap());

        let config = BrokerConfig {
            default_qos: QoSLevel::AtLeastOnce,
            message_ttl: Duration::from_secs(60),
            per_subscriber_queue_capacity: 32,
            max_retries: 3,
            retry_base_delay: Duration::from_millis(50),
            ..Default::default()
        };

        let topic = TopicName::new("ckpt-topic");
        let client_id = ClientId::new("client-A");

        // Pre-crash session: publish 5, ack 4, write checkpoint.
        {
            let broker = Broker::new_with_wal(config.clone(), wal.clone());
            let sub_id = broker.subscribe(client_id.clone(), topic.clone(), QoSLevel::AtLeastOnce);
            for i in 0..5u8 {
                broker
                    .publish_durable(&topic, Bytes::from(vec![i; 4]), QoSLevel::AtLeastOnce)
                    .await
                    .unwrap();
                let polled = broker.poll(sub_id).expect("delivery");
                let tag = polled.delivery_tag.unwrap();
                if i < 4 {
                    assert!(broker.ack(sub_id, tag));
                }
            }
            // Allow spawned ack-journal writes to flush.
            tokio::time::sleep(Duration::from_millis(50)).await;
            wal.flush().await.unwrap();

            // Write a checkpoint that captures cursors AT this point.
            broker
                .write_checkpoint_to(&ckpt_path)
                .await
                .expect("write checkpoint");

            let snap = broker.current_snapshot();
            // snapshot_id == min of cursors. With one consumer that has
            // acked the first 4 of 5 publishes, the cursor (and snapshot_id)
            // should be the wal_id of the 4th published message, i.e. 4.
            assert!(
                snap.snapshot_id >= 4,
                "snapshot_id should reflect 4 acked records, got {}",
                snap.snapshot_id,
            );
            let cursor = snap
                .cursors
                .get(&(client_id.as_str().to_string(), topic.as_str().to_string()))
                .copied()
                .unwrap_or(0);
            assert!(
                cursor >= 4,
                "cursor should reflect 4 acked messages, got {cursor}",
            );
        }

        // Post-crash session: load checkpoint, replay; only unacked
        // (=1) message should be re-delivered.
        let loaded = Broker::load_checkpoint_from(&ckpt_path)
            .await
            .expect("load")
            .expect("file present");
        assert!(loaded.snapshot_id >= 4);

        let broker2 = Broker::new_with_wal(config, wal.clone());
        let sub2 = broker2.subscribe(client_id, topic, QoSLevel::AtLeastOnce);
        broker2
            .replay_from_wal_with_checkpoint(Some(loaded))
            .await
            .unwrap();

        let mut redelivered = 0;
        while broker2.poll(sub2).is_some() {
            redelivered += 1;
        }
        assert_eq!(
            redelivered, 1,
            "with the checkpoint skipping the acked tail, only the unacked msg should re-deliver",
        );
    }

    /// SlowConsumerPolicy::DropSubscription: when a push subscriber's
    /// outbound buffer is at/over the threshold, the broker should drop
    /// the offending subscription wholesale so further publishes don't
    /// pile up.
    #[tokio::test]
    async fn slow_consumer_drop_subscription_unsubscribes() {
        let broker = Broker::new(BrokerConfig {
            default_qos: QoSLevel::AtMostOnce,
            message_ttl: Duration::from_secs(60),
            per_subscriber_queue_capacity: 16,
            max_retries: 3,
            retry_base_delay: Duration::from_millis(50),
            slow_consumer_policy: SlowConsumerPolicy::DropSubscription,
            // Very low threshold so a single delivery trips it on the
            // *second* publish (first publish encodes into the buf,
            // second sees buf already past the cap and triggers the
            // policy).
            slow_consumer_buffer_bytes: 1,
            dlq_suffix: None,
        });
        let topic = TopicName::new("slow-consumer");
        let slot = std::sync::Arc::new(PushSlot::new(64));
        let encoder: DeliveryEncoder = std::sync::Arc::new(|buf, _qos, _tag, _topic, payload| {
            buf.extend_from_slice(payload);
        });

        broker.subscribe_with_slot(
            ClientId::new("c"),
            topic.clone(),
            QoSLevel::AtMostOnce,
            1,
            slot.clone(),
            encoder,
        );
        assert_eq!(broker.subscriber_count(), 1);

        // First publish: buf is empty (< threshold), encoded successfully.
        broker.publish(&topic, Bytes::from_static(b"x"), QoSLevel::AtMostOnce);
        // Second publish: buf is now 1 byte (>= threshold), policy fires.
        broker.publish(&topic, Bytes::from_static(b"y"), QoSLevel::AtMostOnce);

        assert!(
            broker.push_dropped_total() >= 1,
            "expected at least one drop, got {}",
            broker.push_dropped_total(),
        );
        assert_eq!(
            broker.subscriber_count(),
            0,
            "DropSubscription should have unsubscribed the slow consumer",
        );
    }

    /// Consumer-group MVP: 4 members in the same group on the same
    /// topic should split N publishes ~evenly, and a non-grouped
    /// subscriber on the same topic should receive ALL of them
    /// (broadcast semantics preserved).
    #[tokio::test]
    async fn consumer_group_round_robin_load_balances() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let broker = Broker::new(BrokerConfig {
            default_qos: QoSLevel::AtMostOnce,
            message_ttl: Duration::from_secs(60),
            per_subscriber_queue_capacity: 16,
            max_retries: 3,
            retry_base_delay: Duration::from_millis(50),
            slow_consumer_policy: SlowConsumerPolicy::DropNewest,
            slow_consumer_buffer_bytes: 16 * 1024 * 1024,
            dlq_suffix: None,
        });
        let topic = TopicName::new("orders");

        // Per-member delivery counter. The encoder closure captures
        // an `Arc<AtomicU64>` so we can read totals after publishing.
        let group_counts: Vec<Arc<AtomicU64>> =
            (0..4).map(|_| Arc::new(AtomicU64::new(0))).collect();
        let broadcast_count = Arc::new(AtomicU64::new(0));

        for (i, gc) in group_counts.iter().enumerate() {
            let counter = gc.clone();
            let slot = Arc::new(PushSlot::new(1024));
            let encoder: DeliveryEncoder = Arc::new(move |_buf, _qos, _tag, _topic, _payload| {
                counter.fetch_add(1, Ordering::Relaxed);
            });
            broker.subscribe_with_slot_in_group(
                ClientId::new(format!("c{i}")),
                topic.clone(),
                QoSLevel::AtMostOnce,
                (i + 1) as u64,
                slot,
                encoder,
                "workers".to_string(),
            );
        }

        // Non-grouped broadcast subscriber on the same topic.
        {
            let counter = broadcast_count.clone();
            let slot = Arc::new(PushSlot::new(1024));
            let encoder: DeliveryEncoder = Arc::new(move |_buf, _qos, _tag, _topic, _payload| {
                counter.fetch_add(1, Ordering::Relaxed);
            });
            broker.subscribe_with_slot(
                ClientId::new("broadcaster"),
                topic.clone(),
                QoSLevel::AtMostOnce,
                100,
                slot,
                encoder,
            );
        }

        const N: u64 = 1000;
        for _ in 0..N {
            broker.publish(&topic, Bytes::from_static(b"job"), QoSLevel::AtMostOnce);
        }

        // Each group member should get ~N/4 = 250 deliveries. With
        // strict round-robin (no concurrent publishers in this test)
        // the distribution is exactly even.
        let totals: Vec<u64> = group_counts
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();
        let group_total: u64 = totals.iter().sum();
        assert_eq!(
            group_total, N,
            "group should have received N total deliveries, got {totals:?}"
        );
        for (i, t) in totals.iter().enumerate() {
            assert_eq!(
                *t,
                N / 4,
                "member {i} should have received exactly N/4=250, got {t} (totals: {totals:?})",
            );
        }

        // Broadcast subscriber gets ALL publishes regardless of the
        // group's load-balancing.
        assert_eq!(
            broadcast_count.load(Ordering::Relaxed),
            N,
            "broadcast subscriber should see every publish, not just one per group",
        );
    }

    /// Two consumer groups on the same topic each get one delivery
    /// per publish, independently load-balanced inside each group.
    #[tokio::test]
    async fn two_consumer_groups_each_get_one_delivery_per_publish() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let broker = Broker::new(BrokerConfig {
            default_qos: QoSLevel::AtMostOnce,
            message_ttl: Duration::from_secs(60),
            per_subscriber_queue_capacity: 16,
            max_retries: 3,
            retry_base_delay: Duration::from_millis(50),
            slow_consumer_policy: SlowConsumerPolicy::DropNewest,
            slow_consumer_buffer_bytes: 16 * 1024 * 1024,
            dlq_suffix: None,
        });
        let topic = TopicName::new("events");

        let group_a = Arc::new(AtomicU64::new(0));
        let group_b = Arc::new(AtomicU64::new(0));
        for (gname, counter) in [("group-a", &group_a), ("group-b", &group_b)] {
            // Two members per group so each group can load-balance.
            for i in 0..2 {
                let c = counter.clone();
                let slot = Arc::new(PushSlot::new(1024));
                let encoder: DeliveryEncoder =
                    Arc::new(move |_buf, _qos, _tag, _topic, _payload| {
                        c.fetch_add(1, Ordering::Relaxed);
                    });
                broker.subscribe_with_slot_in_group(
                    ClientId::new(format!("{gname}-{i}")),
                    topic.clone(),
                    QoSLevel::AtMostOnce,
                    (gname.len() * 10 + i) as u64,
                    slot,
                    encoder,
                    gname.to_string(),
                );
            }
        }

        const N: u64 = 100;
        for _ in 0..N {
            broker.publish(&topic, Bytes::from_static(b"e"), QoSLevel::AtMostOnce);
        }

        assert_eq!(
            group_a.load(Ordering::Relaxed),
            N,
            "group-a should have N total"
        );
        assert_eq!(
            group_b.load(Ordering::Relaxed),
            N,
            "group-b should have N total"
        );
    }
}
