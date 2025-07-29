use dashmap::DashMap;
use std::sync::Arc;
use tracing::debug;

use crate::config::CONFIG;
use crate::core::topics::topic::{Topic, TopicName}; // <- import the global config

/// [`TopicRegistry`] is a thread-safe store for managing active topics.
///
/// Uses a DashMap internally for lock-free access, enabling high concurrency.
#[derive(Debug, Default)]
pub struct TopicRegistry {
    topics: DashMap<TopicName, Arc<Topic>>,
}

impl TopicRegistry {
    /// Creates a new empty [`TopicRegistry`].
    pub fn new() -> Self {
        Self {
            topics: DashMap::new(),
        }
    }

    /// Attempts to get an existing topic by name.
    ///
    /// Returns `Some(topic)` if found, or `None` if it does not exist.
    pub fn get_topic(&self, name: &TopicName) -> Option<Arc<Topic>> {
        self.topics.get(name).map(|entry| Arc::clone(&*entry))
    }

    /// Returns an existing topic or creates a new one using the global default capacity.
    pub fn create_or_get_topic(&self, name: &TopicName) -> Arc<Topic> {
        // Pull the default from blipmq.toml → [queues].topic_capacity
        let cap = CONFIG.queues.topic_capacity;
        self.create_or_get_topic_with_capacity(name, cap)
    }

    /// Returns an existing topic or creates a new one with the given queue capacity if it doesn't exist.
    ///
    /// # Arguments
    ///
    /// * `name` - The topic name to retrieve or create.
    /// * `queue_capacity` - The capacity of the topic-level input queue.
    pub fn create_or_get_topic_with_capacity(
        &self,
        name: &TopicName,
        queue_capacity: usize,
    ) -> Arc<Topic> {
        self.topics
            .entry(name.clone())
            .or_insert_with(|| {
                debug!(
                    "📭 Topic '{}' not found; creating new with capacity {}.",
                    name, queue_capacity
                );
                Arc::new(Topic::new(name.clone(), queue_capacity))
            })
            .clone()
    }

    /// Lists all topic names currently registered.
    pub fn list_topics(&self) -> Vec<TopicName> {
        self.topics
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Removes a topic by name.
    ///
    /// Returns `Some(topic)` if it was removed, or `None` if not found.
    pub fn remove_topic(&self, name: &TopicName) -> Option<Arc<Topic>> {
        self.topics.remove(name).map(|(_, topic)| topic)
    }
}
