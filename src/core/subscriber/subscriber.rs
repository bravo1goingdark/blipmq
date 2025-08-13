//! Subscriber module: manages per-subscriber queue and background reactive flush loop.

use crate::config::CONFIG;
use crate::core::error::BlipError;
use crate::core::message::{current_timestamp, encode_message_into, Message};
use crate::core::queue::qos0::Queue;

use crate::core::queue::QueueBehavior;
use bytes::BytesMut;
use std::sync::Arc;
use tokio::io::BufWriter;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, Notify};
use tracing::{error, info};

/// Unique identifier for a subscriber.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubscriberId(pub String);

impl std::fmt::Display for SubscriberId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for SubscriberId {
    fn from(s: String) -> Self {
        SubscriberId(s)
    }
}

impl From<SubscriberId> for String {
    fn from(id: SubscriberId) -> Self {
        id.0
    }
}

/// Represents a connected subscriber with its own message queue,
/// a notification primitive, and a background flush task.
#[derive(Debug)]
pub struct Subscriber {
    id: SubscriberId,
    queue: Arc<Queue>,
    notifier: Arc<Notify>,
}

impl Clone for Subscriber {
    fn clone(&self) -> Self {
        Subscriber {
            id: self.id.clone(),
            queue: Arc::clone(&self.queue),
            notifier: Arc::clone(&self.notifier),
        }
    }
}

impl Subscriber {
    /// Creates a new subscriber and spawns its reactive flush loop using global config.
    ///
    /// # Arguments
    /// * `id` - Subscriber identifier.
    /// * `writer` - Shared, async-locked buffered writer for sending batches.
    pub fn new<W>(id: SubscriberId, writer: Arc<Mutex<BufWriter<W>>>) -> Self
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let capacity = CONFIG.queues.subscriber_capacity;
        let max_batch = CONFIG.delivery.max_batch.max(1); // always >= 1

        // Create bounded SPSC queue and notifier
        let queue = Arc::new(Queue::new(
            id.0.clone(),
            capacity,
            CONFIG.queues.overflow_policy,
        ));
        let notifier = Arc::new(Notify::new());

        // Spawn the reactive flush task
        {
            let queue = Arc::clone(&queue);
            let notifier = Arc::clone(&notifier);
            let id_s = id.clone();
            let writer = Arc::clone(&writer);

            tokio::spawn(async move {
                // Reuse one big buffer; 64 bytes per msg is a decent lower bound for framing.
                let mut out = BytesMut::with_capacity(max_batch * 64);

                loop {
                    // Wait to be notified that at least one message arrived.
                    notifier.notified().await;

                    let now = current_timestamp();
                    let mut encoded = 0usize;

                    // Drain as much as available up to max_batch, *coalescing* multiple wakeups.
                    // We loop until either we hit max_batch or the queue is empty right now.
                    loop {
                        // Pull a chunk (implementation-defined, but up to `remaining`)
                        let remaining = max_batch - encoded;
                        let msgs = queue.drain(remaining);
                        if msgs.is_empty() {
                            break;
                        }

                        for msg in msgs {
                            // TTL check (fast path)
                            if msg.ttl_ms > 0 && now >= msg.timestamp + msg.ttl_ms {
                                continue;
                            }
                            encode_message_into(&msg, &mut out);
                            encoded += 1;
                        }

                        if encoded >= max_batch {
                            break;
                        }
                        // Keep looping immediately to gather any additional messages that arrived
                        // between the last drain and now. This minimizes syscalls.
                    }

                    if encoded == 0 {
                        // Nothing to write (all expired or queue raced to empty). Go back to waiting.
                        out.truncate(0);
                        continue;
                    }

                    // Single critical section: perform exactly one write (and one flush) per batch.
                    // Using BufWriter keeps this efficient; we avoid holding the lock longer than needed.
                    if let Err(e) = async {
                        let mut w = writer.lock().await;
                        w.write_all(&out).await?;
                        // Flushing here maintains low tail latency for subscribers while still batching.
                        w.flush().await
                    }
                    .await
                    {
                        error!("Subscriber {} write/flush error: {:?}", id_s, e);
                        break;
                    }

                    // Reset buffer without freeing capacity.
                    out.truncate(0);
                }

                info!("Flush task ended for subscriber {}", id_s);
            });
        }

        Subscriber {
            id,
            queue,
            notifier,
        }
    }

    /// Enqueues a message into this subscriber's queue and notifies the flush task.
    ///
    /// Returns `Err(BlipError::QueueFull)` if the queue is at capacity.
    pub fn enqueue(&self, msg: Arc<Message>) -> Result<(), BlipError> {
        self.queue.enqueue(msg)?;
        // Wake exactly one waiter (the flush loop). Since it's single consumer, notify_one is ideal.
        self.notifier.notify_one();
        Ok(())
    }

    /// Returns the subscriber's ID.
    pub fn id(&self) -> &SubscriberId {
        &self.id
    }

    /// Expose the internal queue so the broker can push into it.
    pub fn queue(&self) -> Arc<Queue> {
        Arc::clone(&self.queue)
    }
}
