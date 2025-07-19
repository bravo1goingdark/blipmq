use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tracing::error;

use crate::core::topics::topic::{Topic, TopicName};

/// [`TopicRegistry`] is a thread-safe store for managing active topics.
///
/// It allows fast, concurrent access for publishers and subscribers.
/// Each topic is reference-counted using `Arc` and stored in an internal `RwLock<HashMap>`.
#[derive(Debug, Default)]
pub struct TopicRegistry {
    topics: RwLock<HashMap<TopicName, Arc<Topic>>>,
}

impl TopicRegistry {
    /// Creates a new empty [`TopicRegistry`].
    pub fn new() -> Self {
        Self {
            topics: RwLock::new(HashMap::new()),
        }
    }

    /// Attempts to get an existing topic by name.
    ///
    /// Returns `Some(topic)` if found, or `None` if it does not exist
    /// or if the registry is poisoned.
    pub fn get_topic(&self, name: &TopicName) -> Option<Arc<Topic>> {
        match self.topics.read() {
            Ok(topics) => topics.get(name).cloned(),
            Err(e) => {
                error!("🔒 Failed to read TopicRegistry (get_topic): {}", e);
                None
            }
        }
    }

    /// Returns an existing topic or creates a new one if it doesn't exist.
    ///
    /// Uses optimistic read-first, then upgrades to a write lock if needed.
    pub fn create_or_get_topic(&self, name: &TopicName) -> Arc<Topic> {
        // read first
        if let Ok(topics) = self.topics.read() {
            if let Some(existing) = topics.get(name) {
                return Arc::clone(existing);
            } else {
                tracing::debug!("📭 Topic '{}' not found; creating new.", name);
            }
        } else {
            error!("🔒 Failed to read TopicRegistry (create_or_get_topic)");
        }

        // upgrade to write lock to insert
        match self.topics.write() {
            Ok(mut topics) => topics
                .entry(name.clone())
                .or_insert_with(|| Arc::new(Topic::new(name.clone())))
                .clone(),
            Err(e) => {
                error!(
                    "🔒 Failed to write TopicRegistry (create_or_get_topic): {}",
                    e
                );
                // Return a dummy topic
                // For now, we’ll return a new topic (not inserted into map).
                Arc::new(Topic::new(name.clone()))
            }
        }
    }

    /// Lists all topic names currently registered.
    pub fn list_topics(&self) -> Vec<TopicName> {
        match self.topics.read() {
            Ok(topics) => topics.keys().cloned().collect(),
            Err(e) => {
                error!("🔒 Failed to read TopicRegistry (list_topics): {}", e);
                vec![]
            }
        }
    }

    /// Removes a topic by name.
    ///
    /// Returns `Some(topic)` if it was removed, or `None` if not found or on lock error.
    pub fn remove_topic(&self, name: &TopicName) -> Option<Arc<Topic>> {
        match self.topics.write() {
            Ok(mut topics) => topics.remove(name),
            Err(e) => {
                error!("🔒 Failed to write TopicRegistry (remove_topic): {}", e);
                None
            }
        }
    }
}
