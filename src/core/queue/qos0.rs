use std::sync::Arc;
use crossbeam_channel::Sender;

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
    fn enqueue(&self, message: Arc<Message>) -> Result<(), ()> {
        self.sender.try_send(message).map_err(|_| ())
    }

    fn dequeue(&self) -> Option<Arc<Message>> {
        None
    }
    fn len(&self) -> usize {
        0
    }

    fn name(&self) -> &str {
        &self.name
    }
}
