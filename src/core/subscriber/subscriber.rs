use std::fmt;
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

use crate::core::message::Message;

/// Unique identifier for a subscriber.
/// Used for routing messages to the correct subscriber queues.
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

/// Represents a connected subscriber to a topic.
///
/// Each subscriber has:
/// - a unique ID (used for registration and routing),
/// - an optional sender to deliver messages via a channel.
#[derive(Debug)]
pub struct Subscriber {
    id: SubscriberId,
    sender: Option<UnboundedSender<Arc<Message>>>,
}

impl Subscriber {
    /// Creates a new subscriber instance with the given ID and message channel.
    #[inline(always)]
    pub fn new(id: SubscriberId, sender: UnboundedSender<Arc<Message>>) -> Self {
        Self {
            id,
            sender: Some(sender),
        }
    }

    /// Returns a reference to the subscriber's ID.
    #[inline(always)]
    pub fn id(&self) -> &SubscriberId {
        &self.id
    }

    /// Returns a reference to the sender channel, if connected.
    #[inline(always)]
    pub fn sender(&self) -> Option<&UnboundedSender<Arc<Message>>> {
        self.sender.as_ref()
    }

    /// Replaces the internal sender with a new one.
    #[inline(always)]
    pub fn set_sender(&mut self, new_sender: UnboundedSender<Arc<Message>>) {
        self.sender = Some(new_sender);
    }

    /// Returns whether the subscriber is still connected.
    #[inline(always)]
    pub fn is_connected(&self) -> bool {
        self.sender
            .as_ref()
            .map(|s| !s.is_closed())
            .unwrap_or(false)
    }

    /// Disconnects the subscriber by dropping its sender.
    #[inline(always)]
    pub fn disconnect(&mut self) {
        self.sender = None;
    }

    /// Tries to send a message to the subscriber.
    /// Returns Ok(()) if successful, Err(()) otherwise.
    #[inline(always)]
    pub fn try_send(&self, msg: Arc<Message>) -> Result<(), ()> {
        if let Some(sender) = &self.sender {
            sender.send(msg).map_err(|_| ())
        } else {
            Err(())
        }
    }
}
