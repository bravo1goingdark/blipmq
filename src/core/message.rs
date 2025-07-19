use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Represents a message to be passed through the broker.
/// Each message has a unique ID, a binary payload, and a timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Message {
    pub id: String,
    pub payload: Vec<u8>,
    pub timestamp: u64,
}

impl Message {
    /// Creates a new message with a generated UUID and current timestamp.
    ///
    /// # Arguments
    ///
    /// * `payload` - The binary content of the message.
    pub fn new(payload: impl Into<Vec<u8>>) -> Self {
        let id = Uuid::new_v4().to_string();
        let timestamp = current_timestamp();
        Self {
            id,
            payload: payload.into(),
            timestamp,
        }
    }

    /// Creates a new message with a custom ID.
    ///
    /// # Arguments
    ///
    /// * `id` - A user-specified unique identifier.
    /// * `payload` - The binary content of the message.
    /// * `timestamp` - Unix timestamp in milliseconds.
    pub fn with_custom(id: impl Into<String>, payload: impl Into<Vec<u8>>, timestamp: u64) -> Self {
        Self {
            id: id.into(),
            payload: payload.into(),
            timestamp,
        }
    }
}

/// Returns the current system time as a UNIX timestamp in milliseconds.
fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis() as u64
}
