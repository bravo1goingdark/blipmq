use bytes::{Buf, BufMut, Bytes, BytesMut};
use flatbuffers::{root, FlatBufferBuilder, InvalidFlatbuffer};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::generated::blipmq;

#[derive(Debug, Clone)]
pub struct Message {
    pub id: u64,
    pub payload: Bytes,
    pub timestamp: u64,
    pub ttl_ms: u64,
}

/// Pre-encoded wire representation for a message frame.
/// Holds the full frame bytes (4-byte length prefix + type + FlatBuffer payload)
/// and the absolute expiration time in milliseconds since epoch (0 = never expire).
#[derive(Debug, Clone)]
pub struct WireMessage {
    pub frame: Bytes,
    pub expire_at: u64,
}

impl WireMessage {
    #[inline]
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expire_at != 0 && now_ms >= self.expire_at
    }
}

#[derive(Debug, Clone)]
pub struct SubAck {
    pub topic: String,
    pub info: String,
}

#[derive(Debug, Clone)]
pub enum ServerFrame {
    SubAck(SubAck),
    Message(Message),
}

const FRAME_SUBACK: u8 = 0;
pub(crate) const FRAME_MESSAGE: u8 = 1;

pub fn new_message(payload: impl Into<Bytes>) -> Message {
    new_message_with_ttl(payload, 0)
}

pub fn new_message_with_ttl(payload: impl Into<Bytes>, ttl_ms: u64) -> Message {
    Message {
        id: generate_id(),
        payload: payload.into(),
        timestamp: current_timestamp(),
        ttl_ms,
    }
}

pub fn current_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis() as u64
}

pub fn with_custom_message(
    id: u64,
    payload: impl Into<Bytes>,
    timestamp: u64,
    ttl_ms: u64,
) -> Message {
    Message {
        id,
        payload: payload.into(),
        timestamp,
        ttl_ms,
    }
}

/// Generates a monotonically increasing u64 ID (fast, lock-free).
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
fn generate_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Serialize a raw `Message` (no length-prefix).
pub fn encode_message(msg: &Message) -> Vec<u8> {
    let mut builder = FlatBufferBuilder::new();
    let payload = builder.create_vector(msg.payload.as_ref());
    let args = blipmq::MessageArgs {
        id: msg.id,
        payload: Some(payload),
        timestamp: msg.timestamp,
        ttl_ms: msg.ttl_ms,
    };
    let offset = blipmq::Message::create(&mut builder, &args);
    builder.finish(offset, None);
    builder.finished_data().to_vec()
}

/// Deserialize a raw `Message` (no length-prefix).
pub fn decode_message(bytes: &[u8]) -> Result<Message, InvalidFlatbuffer> {
    let m = root::<blipmq::Message>(bytes)?;
    Ok(Message {
        id: m.id(),
        payload: m
            .payload()
            .map(|v| bytes::Bytes::from(v.iter().collect::<Vec<_>>()))
            .unwrap_or_else(Bytes::new),
        timestamp: m.timestamp(),
        ttl_ms: m.ttl_ms(),
    })
}

/// Encodes a `Message` with 4-byte length prefix into buffer.
pub fn encode_message_into(msg: &Message, buf: &mut BytesMut) {
    let data = encode_message(msg);
    buf.reserve(4 + data.len());
    buf.put_u32(data.len() as u32);
    buf.extend_from_slice(&data);
}

/// Encodes a `Message` as a typed ServerFrame::Message with 4-byte length prefix.
pub fn encode_message_frame_into(msg: &Message, buf: &mut BytesMut) {
    let data = encode_message(msg);
    buf.reserve(4 + 1 + data.len());
    buf.put_u32((1 + data.len()) as u32);
    buf.put_u8(FRAME_MESSAGE);
    buf.extend_from_slice(&data);
}

/// Builds and returns an owned frame (4-byte length prefix + type + payload) for a Message.
#[inline]
pub fn encode_message_frame(msg: &Message) -> Bytes {
    let data = encode_message(msg);
    let mut buf = BytesMut::with_capacity(4 + 1 + data.len());
    buf.put_u32((1 + data.len()) as u32);
    buf.put_u8(FRAME_MESSAGE);
    buf.extend_from_slice(&data);
    buf.freeze()
}

/// Converts a Message into a pre-encoded wire frame along with its expiration time.
#[inline]
pub fn to_wire_message(msg: &Message) -> WireMessage {
    let frame = encode_message_frame(msg);
    let expire_at = if msg.ttl_ms == 0 { 0 } else { msg.timestamp + msg.ttl_ms };
    WireMessage { frame, expire_at }
}

/// Encodes a `SubAck` with length prefix into buffer.
pub fn encode_suback(ack: &SubAck) -> Vec<u8> {
    let mut builder = FlatBufferBuilder::new();
    let topic = builder.create_string(&ack.topic);
    let info = builder.create_string(&ack.info);
    let args = blipmq::SubAckArgs {
        topic: Some(topic),
        info: Some(info),
    };
    let offset = blipmq::SubAck::create(&mut builder, &args);
    builder.finish(offset, None);
    builder.finished_data().to_vec()
}

fn decode_suback(bytes: &[u8]) -> Result<SubAck, InvalidFlatbuffer> {
    let s = root::<blipmq::SubAck>(bytes)?;
    Ok(SubAck {
        topic: s.topic().unwrap_or("").to_string(),
        info: s.info().unwrap_or("").to_string(),
    })
}
/// Encodes a `ServerFrame` with length prefix into buffer.
pub fn encode_frame_into(frame: &ServerFrame, buf: &mut BytesMut) {
    match frame {
        ServerFrame::SubAck(ack) => {
            let data = encode_suback(ack);
            buf.reserve(4 + 1 + data.len());
            buf.put_u32((1 + data.len()) as u32);
            buf.put_u8(FRAME_SUBACK);
            buf.extend_from_slice(&data);
        }
        ServerFrame::Message(msg) => {
            let data = encode_message(msg);
            buf.reserve(4 + 1 + data.len());
            buf.put_u32((1 + data.len()) as u32);
            buf.put_u8(FRAME_MESSAGE);
            buf.extend_from_slice(&data);
        }
    }
}

/// Deserialize a `ServerFrame` from raw bytes.
pub fn decode_frame(bytes: &[u8]) -> Result<ServerFrame, InvalidFlatbuffer> {
    if bytes.is_empty() {
        return Err(InvalidFlatbuffer::MissingRequiredField {
            required: "frame".into(),
            error_trace: Default::default(),
        });
    }
    let t = bytes[0];
    let payload = &bytes[1..];
    match t {
        FRAME_SUBACK => decode_suback(payload).map(ServerFrame::SubAck),
        FRAME_MESSAGE => decode_message(payload).map(ServerFrame::Message),
        _ => Err(InvalidFlatbuffer::InconsistentUnion {
            field: "frame".into(),
            field_type: "type".into(),
            error_trace: Default::default(),
        }),
    }
}

/// Extracts a length-prefixed `ServerFrame` from a buffer.
pub fn extract_frame(buf: &mut BytesMut) -> Option<Result<ServerFrame, InvalidFlatbuffer>> {
    if buf.len() < 4 {
        return None;
    }

    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        return None;
    }

    // Remove length prefix
    buf.advance(4);
    let payload = buf.split_to(len);
    Some(decode_frame(&payload))
}
