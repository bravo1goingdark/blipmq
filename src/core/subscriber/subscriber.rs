use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

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
            receiver: self.receiver.clone(),
        }
    }
}

/// Represents a connected subscriber to a topic.
#[derive(Debug)]
pub struct Subscriber {
    id: SubscriberId,
    sender: Sender<Arc<Message>>,
    receiver: Receiver<Arc<Message>>,
}

impl Subscriber {
    /// Create a new subscriber with a fresh unbounded crossbeam channel.
    #[inline(always)]
    pub fn new(id: SubscriberId) -> Self {
        let (tx, rx) = unbounded();
        Self {
            id,
            sender: tx,
            receiver: rx,
        }
    }

    /// Use bounded channel version
    pub fn new_bounded(id: SubscriberId, capacity: usize) -> Self {
        let (tx, rx) = bounded(capacity);
        Self {
            id,
            sender: tx,
            receiver: rx,
        }
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
    pub fn receiver(&self) -> &Receiver<Arc<Message>> {
        &self.receiver
    }

    /// Consumes and returns the receiver (for async bridge)
    #[inline(always)]
    pub fn take_receiver(self) -> Receiver<Arc<Message>> {
        self.receiver
    }

    #[inline(always)]
    pub fn try_send(&self, msg: Arc<Message>) -> Result<(), ()> {
        self.sender.try_send(msg).map_err(|_| ())
    }

    // Creates a subscriber and returns the receiver separately
    pub fn new_with_receiver(id: SubscriberId) -> (Self, Receiver<Arc<Message>>) {
        let (tx, rx) = unbounded();
        let subscriber = Subscriber {
            id,
            sender: tx.clone(),
            receiver: rx.clone(), // used once for internal tracking
        };
        (subscriber, rx)
    }
}
