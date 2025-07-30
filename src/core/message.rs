//! Protobuf-backed types and serialization utilities for BlipMQ.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use prost::Message as ProstMessage;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/blipmq.rs"));
}

pub use proto::{Message, ServerFrame, SubAck};

/// Create a new `Message` with payload and default TTL (0).
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

/// Returns current UNIX timestamp in milliseconds.
pub fn current_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis() as u64
}

/// Create a custom `Message` with fixed ID/timestamp/TTL.
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

/// Generates a random u64 ID using UUID v4 (lower 64 bits).
fn generate_id() -> u64 {
    use uuid::Uuid;
    let uuid = Uuid::new_v4();
    let bytes = uuid.as_u128().to_be_bytes();
    u64::from_be_bytes(bytes[8..16].try_into().unwrap())
}

/// Serialize a raw `Message` (no length-prefix).
pub fn encode_message(msg: &Message) -> Vec<u8> {
    let mut buf = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut buf).expect("Failed to encode Message");
    buf
}

/// Deserialize a raw `Message` (no length-prefix).
pub fn decode_message(bytes: &[u8]) -> Result<Message, prost::DecodeError> {
    Message::decode(bytes)
}

/// Encodes a `Message` with 4-byte length prefix into buffer.
pub fn encode_message_into(msg: &Message, buf: &mut BytesMut) {
    let len = msg.encoded_len() as u32;
    buf.reserve(len as usize + 4);
    buf.put_u32(len);
    msg.encode(buf)
        .expect("encoding Message into buffer failed");
}

/// Encodes a `SubAck` with length prefix into buffer.
pub fn encode_suback_into(ack: &SubAck, buf: &mut BytesMut) {
    let len = ack.encoded_len() as u32;
    buf.reserve(len as usize + 4);
    buf.put_u32(len);
    ack.encode(buf).expect("encoding SubAck failed");
}

/// Encodes a `ServerFrame` with length prefix into buffer.
pub fn encode_frame_into(frame: &ServerFrame, buf: &mut BytesMut) {
    let len = frame.encoded_len() as u32;
    buf.reserve(len as usize + 4);
    buf.put_u32(len);
    frame.encode(buf).expect("encoding ServerFrame failed");
}

/// Deserialize a `ServerFrame` from raw bytes.
pub fn decode_frame(bytes: &[u8]) -> Result<ServerFrame, prost::DecodeError> {
    ServerFrame::decode(bytes)
}

/// Extracts a length-prefixed `ServerFrame` from a buffer.
pub fn extract_frame(buf: &mut BytesMut) -> Option<Result<ServerFrame, prost::DecodeError>> {
    if buf.len() < 4 {
        return None;
    }

    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        return None;
    }

    // Remove length prefix
    buf.advance(4);
    let mut payload = buf.split_to(len);
    Some(ServerFrame::decode(&mut payload))
}
