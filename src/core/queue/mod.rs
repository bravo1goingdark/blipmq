//! BlipMQ queue module.
//!
//! Defines the core queue abstraction and pluggable queue implementations.
//! Each subscriber gets its own queue.
//!
//! Supports:
//! - Multiple QoS levels (e.g., QoS 0, QoS 1)
//! - Delivery modes (Ordered / Unordered)

pub mod manager;
pub mod qos0;
pub mod qos1;

pub use manager::QueueManager;

use std::fmt::Debug;
use std::sync::Arc;

use crate::core::message::Message;

/// Trait representing the common interface for all queue implementations.
///
/// Enables uniform interaction across QoS implementations (QoS 0, QoS 1).
/// Each queue is tied to a subscriber and handles its own message lifecycle.
///
/// All implementations must be:
/// - Thread-safe (`Send + Sync`)
/// - Clone-efficient (Arc<Message> based)
pub trait QueueBehavior: Send + Sync + Debug {
    /// Enqueue a message into the queue (fan-in).
    fn enqueue(&self, message: Arc<Message>);

    /// Attempt to dequeue a message (fan-out).
    fn dequeue(&self) -> Option<Arc<Message>>;

    /// Return the current number of messages in the queue.
    fn len(&self) -> usize;

    /// Return the queue’s identifier (usually subscriber ID).
    fn name(&self) -> &str;
}
