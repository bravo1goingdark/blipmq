use bytes::{Buf, BufMut, Bytes, BytesMut};
use flatbuffers::{root, FlatBufferBuilder, InvalidFlatbuffer};

mod generated {
    include!(concat!(env!("OUT_DIR"), "/flatbuffers/mod.rs"));
}
use generated::blipmq;

#[derive(Debug, Clone)]
pub struct Message {
    pub id: u64,
    pub payload: Bytes, // zero-copy, ref-counted
    pub timestamp: u64,
    pub ttl_ms: u64,
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
const FRAME_MESSAGE: u8 = 1;

#[inline]
pub fn new_message(payload: impl Into<Bytes>) -> Message {
    new_message_with_ttl(payload, 0)
}

#[inline]
pub fn new_message_with_ttl(payload: impl Into<Bytes>, ttl_ms: u64) -> Message {
    Message {
        id: generate_id(),
        payload: payload.into(),
        timestamp: current_timestamp(),
        ttl_ms,
    }
}

#[inline]
pub fn current_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis() as u64
}

#[inline]
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

/// Generates a random u64 ID using UUID v4 (lower 64 bits).
/// NOTE: UUID is simple but not the cheapest. If ID gen becomes a hotspot,
/// replace with a shard-aware monotonic counter or Snowflake-style ID.
#[inline]
fn generate_id() -> u64 {
    use uuid::Uuid;
    let uuid = Uuid::new_v4();
    let bytes = uuid.as_u128().to_be_bytes();
    u64::from_be_bytes(bytes[8..16].try_into().unwrap())
}

/* -------------------------- Raw Message encoding -------------------------- */

/// Serialize a raw `Message` (no length-prefix).
/// Kept for compatibility; allocates a Vec.
#[inline]
pub fn encode_message(msg: &Message) -> Vec<u8> {
    // Pre-size the builder close to payload length to reduce reallocs.
    let mut builder = FlatBufferBuilder::with_capacity(12 + msg.payload.len());
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
#[inline]
pub fn decode_message(bytes: &[u8]) -> Result<Message, InvalidFlatbuffer> {
    let m = root::<blipmq::Message>(bytes)?;
    let payload = if let Some(v) = m.payload() {
        // Older/newer FlatBuffers: `bytes()` gives &[u8]
        // We must own this beyond the FlatBuffer’s lifetime, so copy once.
        Bytes::copy_from_slice(v.bytes())
    } else {
        Bytes::new()
    };

    Ok(Message {
        id: m.id(),
        payload,
        timestamp: m.timestamp(),
        ttl_ms: m.ttl_ms(),
    })
}

/// Encodes a `Message` with a 4-byte length prefix into `buf`.
/// Avoids intermediate Vec by writing FlatBuffer directly into `buf`.
#[inline]
pub fn encode_message_into(msg: &Message, buf: &mut BytesMut) {
    let mut builder = FlatBufferBuilder::with_capacity(12 + msg.payload.len());
    let payload = builder.create_vector(msg.payload.as_ref());
    let args = blipmq::MessageArgs {
        id: msg.id,
        payload: Some(payload),
        timestamp: msg.timestamp,
        ttl_ms: msg.ttl_ms,
    };
    let offset = blipmq::Message::create(&mut builder, &args);
    builder.finish(offset, None);

    let data = builder.finished_data();
    buf.reserve(4 + data.len());
    buf.put_u32(data.len() as u32);
    buf.extend_from_slice(data);
}

/* --------------------------- SubAck / Frames ------------------------------ */

#[inline]
pub fn encode_suback(ack: &SubAck) -> Vec<u8> {
    let mut builder = FlatBufferBuilder::with_capacity(8 + ack.topic.len() + ack.info.len());
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

#[inline]
fn decode_suback(bytes: &[u8]) -> Result<SubAck, InvalidFlatbuffer> {
    let s = root::<blipmq::SubAck>(bytes)?;
    Ok(SubAck {
        topic: s.topic().unwrap_or("").to_string(),
        info: s.info().unwrap_or("").to_string(),
    })
}

/// Encodes a `ServerFrame` with length prefix into buffer.
/// SubAck is framed as [len][type][flatbuffer].
/// Message frame uses the same structure so you can choose to use framed messages later.
/// (Your subscriber path still uses `encode_message_into` directly, which is fine.)
#[inline]
pub fn encode_frame_into(frame: &ServerFrame, buf: &mut BytesMut) {
    match frame {
        ServerFrame::SubAck(ack) => {
            let mut builder =
                FlatBufferBuilder::with_capacity(8 + ack.topic.len() + ack.info.len());
            let topic = builder.create_string(&ack.topic);
            let info = builder.create_string(&ack.info);
            let args = blipmq::SubAckArgs {
                topic: Some(topic),
                info: Some(info),
            };
            let offset = blipmq::SubAck::create(&mut builder, &args);
            builder.finish(offset, None);

            let data = builder.finished_data();
            buf.reserve(4 + 1 + data.len());
            buf.put_u32((1 + data.len()) as u32);
            buf.put_u8(FRAME_SUBACK);
            buf.extend_from_slice(data);
        }
        ServerFrame::Message(msg) => {
            let mut builder = FlatBufferBuilder::with_capacity(12 + msg.payload.len());
            let payload = builder.create_vector(msg.payload.as_ref());
            let args = blipmq::MessageArgs {
                id: msg.id,
                payload: Some(payload),
                timestamp: msg.timestamp,
                ttl_ms: msg.ttl_ms,
            };
            let offset = blipmq::Message::create(&mut builder, &args);
            builder.finish(offset, None);

            let data = builder.finished_data();
            buf.reserve(4 + 1 + data.len());
            buf.put_u32((1 + data.len()) as u32);
            buf.put_u8(FRAME_MESSAGE);
            buf.extend_from_slice(data);
        }
    }
}

/// Deserialize a `ServerFrame` from raw bytes.
#[inline]
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
#[inline]
pub fn extract_frame(buf: &mut BytesMut) -> Option<Result<ServerFrame, InvalidFlatbuffer>> {
    if buf.len() < 4 {
        return None;
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        return None;
    }

    buf.advance(4);
    let payload = buf.split_to(len);
    Some(decode_frame(&payload))
}
