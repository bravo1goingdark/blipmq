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
        let max_batch = CONFIG.delivery.max_batch;

        // Create bounded SPSC queue and notifier
        let queue = Arc::new(Queue::new(
            id.0.clone(),
            capacity,
            CONFIG.queues.overflow_policy,
        ));
        let notifier = Arc::new(Notify::new());
        let queue_clone = Arc::clone(&queue);
        let notifier_clone = Arc::clone(&notifier);
        let id_clone = id.clone();
        let writer_clone = Arc::clone(&writer);

        // Spawn the reactive flush task
        tokio::spawn(async move {
            let mut buffer = BytesMut::with_capacity(max_batch * 64);
            loop {
                // Wait for at least one notification
                notifier_clone.notified().await;

                let now = current_timestamp();
                let messages = queue_clone.drain(max_batch);

                for msg in messages {
                    // Enforce TTL
                    if msg.ttl_ms > 0 && now >= msg.timestamp + msg.ttl_ms {
                        continue;
                    }
                    encode_message_into(&msg, &mut buffer);
                }

                // Write and flush the batch
                let mut w = writer_clone.lock().await;
                if let Err(e) = w.write_all(&buffer).await {
                    tracing::error!("Subscriber {} write error: {:?}", id_clone, e);
                    break;
                }
                if let Err(e) = w.flush().await {
                    tracing::error!("Subscriber {} flush error: {:?}", id_clone, e);
                    break;
                }

                buffer.clear();
            }
            tracing::info!("Flush task ended for subscriber {}", id_clone);
        });

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
