use std::sync::Arc;
use tokio::sync::mpsc::Sender;

use crate::core::error::BlipError;
use crate::core::message::Message;
use crate::core::queue::QueueBehavior;

#[derive(Debug)]
pub struct Queue {
    name: String,
    sender: Sender<Arc<Message>>,
}

impl Queue {
    pub fn new(name: impl Into<String>, sender: Sender<Arc<Message>>) -> Self {
        Self {
            name: name.into(),
            sender,
        }
    }
}

impl QueueBehavior for Queue {
    fn enqueue(&self, message: Arc<Message>) -> Result<(), BlipError> {
        self.sender.try_send(message).map_err(|e| match e {
            tokio::sync::mpsc::error::TrySendError::Full(_) => BlipError::QueueFull,
            tokio::sync::mpsc::error::TrySendError::Closed(_) => BlipError::Disconnected,
        })
    }
    fn dequeue(&self) -> Option<Arc<Message>> {
        None
    }
    fn len(&self) -> usize {
        0
    }

    fn is_empty(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        &self.name
    }
}
