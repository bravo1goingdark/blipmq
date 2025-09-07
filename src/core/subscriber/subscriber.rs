//! Subscriber module: manages per-subscriber queue and background reactive flush loop.

use crate::config::CONFIG;
use crate::core::error::BlipError;
use crate::core::message::{current_timestamp, WireMessage};
use crate::core::queue::qos0::Queue;
use crate::core::queue::QueueBehavior;

use bytes::BytesMut;
use std::sync::Arc;
use bytes::Bytes;
use tokio::sync::{mpsc, Notify};

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
    /// * `writer_tx` - Channel to the dedicated connection writer task.
    pub fn new(id: SubscriberId, writer_tx: mpsc::Sender<Bytes>) -> Self {
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
        let writer_tx = writer_tx.clone();

        // Spawn the reactive flush task
        tokio::spawn(async move {
            let mut buffer = BytesMut::with_capacity(max_batch * 64);
            'outer: loop {
                // Wait for at least one notification that the queue transitioned
                // from empty -> non-empty.
                notifier_clone.notified().await;

                // Drain until the queue is empty; do not wait for another notify
                // to avoid stalling when more than `max_batch` messages are queued.
                loop {
                    let now = current_timestamp();
                    let messages = queue_clone.drain(max_batch);
                    if messages.is_empty() {
                        break;
                    }

                    for wm in messages {
                        // Enforce TTL
                        if wm.is_expired(now) {
                            continue;
                        }
                        buffer.extend_from_slice(&wm.frame);
                    }

                    // Skip I/O if nothing to flush (e.g., all messages expired)
                    if buffer.is_empty() {
                        // Check if more messages accumulated; if not, break
                        if queue_clone.is_empty() {
                            break;
                        } else {
                            continue;
                        }
                    }

                    // Send the batch to the connection writer task
                    let chunk: Bytes = buffer.split().freeze();
                    crate::metrics::inc_flush_bytes(chunk.len() as u64);
                    crate::metrics::inc_flush_batches(1);
                    match writer_tx.try_send(chunk) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(chunk)) => {
                            if let Err(e) = writer_tx.send(chunk).await {
                                tracing::error!("Subscriber {} writer channel closed: {:?}", id_clone, e);
                                break 'outer;
                            }
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_chunk)) => {
                            tracing::error!("Subscriber {} writer channel closed", id_clone);
                            break 'outer;
                        }
                    }

                    // Continue inner loop to drain remaining messages (if any)
                    // without waiting for another notify.
                }
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
    pub fn enqueue(&self, msg: Arc<WireMessage>) -> Result<(), BlipError> {
        self.queue.enqueue(msg)?;
        // Notify only on transition from empty -> non-empty to reduce wakeups
        if self.queue.len() == 1 {
            self.notifier.notify_one();
        }
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
