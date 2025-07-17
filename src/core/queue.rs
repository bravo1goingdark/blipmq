use std::collections::VecDeque;
use std::fs::Metadata;
use std::sync::{Arc, Mutex};

use super::message::Message;

#[derive(Debug, Clone)]
pub struct Queue {
    name: String,
    messages: Arc<Mutex<VecDeque<Message>>>, // support concurrent pub-sub
}

impl Queue {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            message: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
}

impl Queue {
    pub fn enqueue(&self, message: Message) {
        // we lock the message to get mutable access
        // so if lock fails the message is dropped silently (future : log/retry)
        if let Ok(mut q) = self.messages.lock() {
            q.push_back(message);
        }
    }

    pub fn dequeue(&self) -> Option<Message> {
        // lock ensure no race condition
        // if lock fails, it returns None
        if let Ok(mut q) = self.messages.lock() {
            q.pop_front();
        } else {
            None
        }
    }

    pub fn len(&self) -> usize {
        // returns the number of messages in queue
        // if lock fails -> default:0
        self.messages.lock()
            .map(|q| q.len())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        // check if queue is empty
        self.len() == 0
    }
}
