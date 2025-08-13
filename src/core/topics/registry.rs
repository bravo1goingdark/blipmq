use dashmap::DashMap;
use std::sync::Arc;
use tracing::debug;

use crate::config::CONFIG;
use crate::core::topics::topic::{Topic, TopicName};

/// [`TopicRegistry`] is a thread-safe store for managing active topics.
///
/// Internally uses `DashMap` for high concurrency. The default topic input
/// queue capacity is captured once at construction to avoid repeated global
/// config reads on the hot path.
#[derive(Debug, Default)]
pub struct TopicRegistry {
    topics: DashMap<TopicName, Arc<Topic>>,
    default_topic_capacity: usize,
}

impl TopicRegistry {
    /// Creates a new [`TopicRegistry`], caching the default topic capacity.
    #[inline]
    pub fn new() -> Self {
        Self {
            topics: DashMap::new(),
            default_topic_capacity: CONFIG.queues.topic_capacity,
        }
    }

    /// Attempts to get an existing topic by name.
    ///
    /// Returns `Some(topic)` if found, or `None` if it does not exist.
    #[inline]
    pub fn get_topic(&self, name: &TopicName) -> Option<Arc<Topic>> {
        self.topics.get(name).map(|e| e.value().clone())
    }

    /// Returns an existing topic or creates a new one using the cached default capacity.
    #[inline]
    pub fn create_or_get_topic(&self, name: &TopicName) -> Arc<Topic> {
        self.create_or_get_topic_with_capacity(name, self.default_topic_capacity)
    }

    /// Returns an existing topic or creates a new one with the given queue capacity if it doesn't exist.
    ///
    /// # Arguments
    /// * `name` - The topic name to retrieve or create.
    /// * `queue_capacity` - The capacity of the topic-level input queue.
    #[inline]
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
    #[inline]
    pub fn list_topics(&self) -> Vec<TopicName> {
        // Reserve approximately to avoid reallocations on large registries.
        let mut out = Vec::with_capacity(self.topics.len());
        for entry in self.topics.iter() {
            out.push(entry.key().clone());
        }
        out
    }

    /// Removes a topic by name.
    ///
    /// Returns `Some(topic)` if it was removed, or `None` if not found.
    #[inline]
    pub fn remove_topic(&self, name: &TopicName) -> Option<Arc<Topic>> {
        self.topics.remove(name).map(|(_, topic)| topic)
    }
}
