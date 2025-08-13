use crate::core::error::BlipError;
use crate::core::message::Message;
use crate::core::queue::QueueBehavior;
use crossbeam_queue::ArrayQueue;
use serde::Deserialize;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::{hint, thread};

/// Defines how a QoS0 queue handles overflow.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverflowPolicy {
    /// Drop the incoming message when the queue is full.
    DropNew,
    /// Drop one oldest entry, then try to insert the new one (constant time).
    DropOldest,
    /// Busy-wait with progressive backoff until space is available (use sparingly).
    Block,
}

impl Default for OverflowPolicy {
    fn default() -> Self {
        OverflowPolicy::DropOldest
    }
}

/// A single-consumer, lock-free, bounded queue for QoS 0 delivery.
///
/// Internally uses `crossbeam_queue::ArrayQueue<Arc<Message>>` which is MPMC.
/// In our architecture, each queue is **MPSC (multi-producer, single-consumer)**:
/// - Producers: topic fanout tasks from multiple topics the subscriber is on
/// - Consumer: the subscriber’s flush loop
#[derive(Debug)]
pub struct Queue {
    name: String,
    queue: Arc<ArrayQueue<Arc<Message>>>,
    policy: OverflowPolicy,

    // Lightweight counters for observability
    enqueued_ok: AtomicU64,
    dropped_new: AtomicU64,
    dropped_oldest: AtomicU64,
}

impl Queue {
    /// Creates a new `Queue` with a given name, capacity, and overflow policy.
    #[inline]
    pub fn new(name: impl Into<String>, capacity: usize, policy: OverflowPolicy) -> Self {
        Self {
            name: name.into(),
            queue: Arc::new(ArrayQueue::new(capacity)),
            policy,
            enqueued_ok: AtomicU64::new(0),
            dropped_new: AtomicU64::new(0),
            dropped_oldest: AtomicU64::new(0),
        }
    }

    /// Returns a cloneable reference to the internal `ArrayQueue`.
    #[inline]
    pub fn queue(&self) -> Arc<ArrayQueue<Arc<Message>>> {
        Arc::clone(&self.queue)
    }

    /// Capacity of this queue.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.queue.capacity()
    }

    /// Drains up to `max` messages for batch flushing.
    ///
    /// Reuses the allocation pattern by pre-reserving `max`.
    #[inline]
    pub fn drain(&self, max: usize) -> Vec<Arc<Message>> {
        let mut drained = Vec::with_capacity(max);
        let mut n = 0;
        while n < max {
            if let Some(msg) = self.queue.pop() {
                drained.push(msg);
                n += 1;
            } else {
                break;
            }
        }
        drained
    }

    /// Snapshot of counters (cheap and lock-free).
    #[inline]
    pub fn metrics(&self) -> (u64, u64, u64) {
        (
            self.enqueued_ok.load(Ordering::Relaxed),
            self.dropped_new.load(Ordering::Relaxed),
            self.dropped_oldest.load(Ordering::Relaxed),
        )
    }
}

impl QueueBehavior for Queue {
    #[inline]
    fn enqueue(&self, message: Arc<Message>) -> Result<(), BlipError> {
        // Fast-path: try push once.
        if self.queue.push(message.clone()).is_ok() {
            self.enqueued_ok.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        // Overflow handling
        match self.policy {
            OverflowPolicy::DropNew => {
                self.dropped_new.fetch_add(1, Ordering::Relaxed);
                Err(BlipError::QueueFull)
            }

            OverflowPolicy::DropOldest => {
                // Constant-time mitigation: pop **one** and retry once.
                let _ = self.queue.pop();
                if self.queue.push(message).is_ok() {
                    self.dropped_oldest.fetch_add(1, Ordering::Relaxed);
                    self.enqueued_ok.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                } else {
                    // Queue likely refilled between pop and push in MPSC contention;
                    // treat as full to avoid spinning.
                    self.dropped_new.fetch_add(1, Ordering::Relaxed);
                    Err(BlipError::QueueFull)
                }
            }

            OverflowPolicy::Block => {
                // Low-latency busy-wait with progressive backoff.
                // Avoids coarse 1ms sleeps which kill tail latency.
                let mut spins = 0u32;
                loop {
                    // Try immediately
                    if self.queue.push(message.clone()).is_ok() {
                        self.enqueued_ok.fetch_add(1, Ordering::Relaxed);
                        return Ok(());
                    }

                    // Exponential-ish backoff: spin a bit, then yield the CPU.
                    if spins < 128 {
                        hint::spin_loop();
                        spins += 1;
                    } else {
                        // Let the scheduler run the consumer.
                        thread::yield_now();
                        // Reset or continue yielding; simple strategy works well here.
                    }
                }
            }
        }
    }

    #[inline]
    fn dequeue(&self) -> Option<Arc<Message>> {
        self.queue.pop()
    }

    #[inline]
    fn len(&self) -> usize {
        self.queue.len()
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    #[inline]
    fn name(&self) -> &str {
        &self.name
    }
}
