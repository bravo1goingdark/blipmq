//! Protobuf-backed types and serialization utilities for BlipMQ.

use bytes::{BufMut, Bytes, BytesMut};
use prost::Message as ProstMessage;
use std::io::Cursor;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/blipmq.rs"));
}

// Re-export generated types
pub use proto::{Message, ServerFrame, SubAck};

/// Utility to create a new `Message` with payload and default TTL (0).
pub fn new_message(payload: impl Into<Bytes>) -> Message {
    new_message_with_ttl(payload, 0)
}

/// Create a new `Message` with a specific TTL in milliseconds.
pub fn new_message_with_ttl(payload: impl Into<Bytes>, ttl_ms: u64) -> Message {
    Message {
        id: generate_id(),
        payload: payload.into().to_vec(),
        timestamp: current_timestamp(),
        ttl_ms,
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
pub fn with_custom_message(
    id: u64,
    payload: impl Into<Bytes>,
    timestamp: u64,
    ttl_ms: u64,
) -> Message {
    Message {
        id,
        payload: payload.into().to_vec(),
        timestamp,
        ttl_ms,
    }
}

/// Generates a random u64 ID using UUID v4 (lower 64 bits)
fn generate_id() -> u64 {
    use uuid::Uuid;
    let uuid = Uuid::new_v4();
    let bytes = uuid.as_u128().to_be_bytes();
    u64::from_be_bytes(bytes[8..16].try_into().unwrap())
}

/// Serialize a raw `Message` (no length-prefix) to bytes for TCP transmission.
pub fn encode_message(msg: &Message) -> Vec<u8> {
    let mut buf = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut buf).expect("Failed to encode Message");
    buf
}

/// Deserialize a raw `Message` (no length-prefix) from bytes (from TCP stream).
pub fn decode_message(bytes: &[u8]) -> Result<Message, prost::DecodeError> {
    Message::decode(Cursor::new(bytes))
}

/// Encodes the `Message` into the provided `BytesMut` buffer,
/// prefixing with a 4-byte big-endian length header.
pub fn encode_message_into(msg: &Message, buf: &mut BytesMut) {
    let len = msg.encoded_len() as u32;
    buf.put_u32(len);
    ProstMessage::encode(msg, buf).expect("encoding Message into buffer failed");
}

/// Encodes a `SubAck` into the provided `BytesMut`, length-prefixed.
pub fn encode_suback_into(ack: &SubAck, buf: &mut BytesMut) {
    let len = ack.encoded_len() as u32;
    buf.put_u32(len);
    ProstMessage::encode(ack, buf).expect("encoding SubAck failed");
}

/// Encodes a `ServerFrame` into the provided `BytesMut`, length-prefixed.
pub fn encode_frame_into(frame: &ServerFrame, buf: &mut BytesMut) {
    let len = frame.encoded_len() as u32;
    buf.put_u32(len);
    ProstMessage::encode(frame, buf).expect("encoding ServerFrame failed");
}

/// Deserialize a `ServerFrame` from bytes (after length-prefix removed).
pub fn decode_frame(bytes: &[u8]) -> Result<ServerFrame, prost::DecodeError> {
    ServerFrame::decode(Cursor::new(bytes))
}

/// Utility to decode a length-prefixed frame from a `Buf` and return the inner payload.
///
/// This function expects the buffer to contain at least 4 bytes of length prefix + payload.
pub fn extract_frame(buf: &mut BytesMut) -> Option<Result<ServerFrame, prost::DecodeError>> {
    if buf.len() < 4 {
        return None;
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        return None;
    }
    // split off frame
    let _ = buf.split_to(4); // remove length prefix
    let payload = buf.split_to(len);
    Some(ServerFrame::decode(Cursor::new(&payload)))
}
