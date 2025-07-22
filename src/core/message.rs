//! Protobuf-backed Message struct for BlipMQ.

use bytes::Bytes;
use prost::Message as ProstMessage;
use std::io::Cursor;

/// Generated Protobuf types
pub mod proto {
    // This brings in the generated BlipMessage struct
    include!(concat!(env!("OUT_DIR"), "/blipmq.rs"));
}

pub use proto::Message;

/// Utility to create a new `Message` with payload.
pub fn new_message(payload: impl Into<Bytes>) -> Message {
    Message {
        id: generate_id(),
        payload: payload.into().to_vec(),
        timestamp: current_timestamp(),
    }
}

/// Returns the current system time as a UNIX timestamp in milliseconds.
pub fn current_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("System time is before Unix epoch")
        .as_millis() as u64
}

/// Utility to create a custom message with fixed ID/timestamp
pub fn with_custom_message(id: u64, payload: impl Into<Bytes>, timestamp: u64) -> Message {
    Message {
        id,
        payload: payload.into().to_vec(),
        timestamp,
    }
}

/// Generates a random u64 ID using UUID v4 (lower 64 bits)
fn generate_id() -> u64 {
    use uuid::Uuid;
    let uuid = Uuid::new_v4();
    let bytes = uuid.as_u128().to_be_bytes();
    u64::from_be_bytes(bytes[8..16].try_into().unwrap())
}

/// Serialize the message to bytes (for TCP transmission)
pub fn encode_message(msg: &Message) -> Vec<u8> {
    let mut buf = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut buf).expect("Failed to encode Message");
    buf
}

/// Deserialize a message from bytes (from TCP stream)
pub fn decode_message(bytes: &[u8]) -> Result<Message, prost::DecodeError> {
    Message::decode(Cursor::new(bytes))
}
