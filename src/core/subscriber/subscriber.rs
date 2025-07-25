use std::fmt;
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::mpsc::{self, Receiver, Sender};

/// Default capacity for per-subscriber message queues
pub const DEFAULT_QUEUE_CAPACITY: usize = 1024;

use crate::core::error::BlipError;
use crate::core::message::Message;

/// Unique identifier for a subscriber.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubscriberId(pub String);

impl fmt::Display for SubscriberId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SubscriberId {
    fn from(s: &str) -> Self {
        SubscriberId(s.to_owned())
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

impl AsRef<str> for SubscriberId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Deref for SubscriberId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Clone for Subscriber {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            sender: self.sender.clone(),
        }
    }
}

/// Represents a connected subscriber to a topic.
#[derive(Debug)]
pub struct Subscriber {
    id: SubscriberId,
    sender: Sender<Arc<Message>>,
}

impl Subscriber {
    /// Create a new subscriber and return its receiver.
    #[inline(always)]
    pub fn new(id: SubscriberId) -> (Self, Receiver<Arc<Message>>) {
        Self::with_capacity(id, DEFAULT_QUEUE_CAPACITY)
    }

    /// Use bounded channel version (currently unused)
    pub fn new_bounded(id: SubscriberId, capacity: usize) -> (Self, Receiver<Arc<Message>>) {
        Self::with_capacity(id, capacity)
    }

    /// Internal helper to create a subscriber with specified capacity.
    fn with_capacity(id: SubscriberId, capacity: usize) -> (Self, Receiver<Arc<Message>>) {
        let (tx, rx) = mpsc::channel(capacity);
        (Self { id, sender: tx }, rx)
    }

    #[inline(always)]
    pub fn id(&self) -> &SubscriberId {
        &self.id
    }

    #[inline(always)]
    pub fn sender(&self) -> &Sender<Arc<Message>> {
        &self.sender
    }

    #[inline(always)]
    pub fn try_send(&self, msg: Arc<Message>) -> Result<(), BlipError> {
        self.sender.try_send(msg).map_err(|_| BlipError::QueueFull)
    }

    // Creates a subscriber and returns the receiver separately
    pub fn new_with_receiver(id: SubscriberId) -> (Self, Receiver<Arc<Message>>) {
        Self::new(id)
    }
    /// Creates a subscriber with a custom queue capacity and returns the receiver.
    pub fn new_with_receiver_capacity(
        id: SubscriberId,
        capacity: usize,
    ) -> (Self, Receiver<Arc<Message>>) {
        Self::with_capacity(id, capacity)
    }
}
