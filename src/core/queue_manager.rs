use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::message::Message;
use super::queue::Queue;

#[derive(Debug, Clone)]
pub struct QueueManager {
    // shared map of queues names to queue instances
    // {Arc , RwLock} allow concurrent access & mutations
    queues: Arc<RwLock<HashMap<String, Queue>>>,
}

impl QueueManager {
    // creates an empty QueueManager
    // map is protected with RwLock -> ensures safe access across multiple threads
    pub fn new() -> Self {
        Self {
            queues: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    // adds a queue only if it doesn't already exist
    // uses write() lock to mutate the map
    pub fn create_queue(&self, name: &str) {
        let mut queues = self.queues.write().unwrap();
        queues
            .entry(name.to_string())
            .or_insert_with(|| Queue::new(name))
    }

    // enq a message into named queue
    // read() lock -> access value safely
    // returns true if success else false
    pub fn enqueue(&self, queue_name: &str, message: Message) -> bool {
        let queues = self.queues.read().unwrap();
        if let Some(queue) = queues.get(queue_name) {
            queue.enqueue(message);
            true
        } else {
            false
        }
    }

    // deq a message from a named queue
    // read() lock -> access value safely
    // returns None if the queue doesn't exist or is empty
    pub fn dequeue(&self, queue_name: &str) -> Option<Message> {
        let queues = self.queues.read().unwrap();
        queues
            .get(queue_name)
            .and_then(|queue| queue.dequeue())
    }

    // return number of messages in a specific queue
    // useful for monitoring or debugging queue health
    pub fn get_queue_len(&self , queue_name : &str) -> Option<usize> {
        let queues = self.queues.read().unwrap();
        queues
            .get(queue_name)
            .map(|queue| queue.len())
    }


    // list all existing queues (only names)
    // later maybe used in API's/DashBoard
    pub fn list_queues(&self) -> Vec<String> {
        let queues = self.queues.read().unwrap();
        queues
            .keys()
            .cloned()
            .collect()
    }
}
