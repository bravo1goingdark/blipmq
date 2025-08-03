use bytes::{Buf, BufMut, Bytes, BytesMut};
use flatbuffers::{root, FlatBufferBuilder, InvalidFlatbuffer};

mod generated {
    include!(concat!(env!("OUT_DIR"), "/flatbuffers/mod.rs"));
}
use generated::blipmq;

#[derive(Debug, Clone)]
pub struct Message {
    pub id: u64,
    pub payload: Vec<u8>,
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

pub fn new_message(payload: impl Into<Bytes>) -> Message {
    new_message_with_ttl(payload, 0)
}

pub fn new_message_with_ttl(payload: impl Into<Bytes>, ttl_ms: u64) -> Message {
    Message {
        id: generate_id(),
        payload: payload.into().to_vec(),
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
    let mut builder = FlatBufferBuilder::new();
    let payload = builder.create_vector(&msg.payload);
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
            .map(|v| v.iter().collect::<Vec<_>>())
            .unwrap_or_default(),
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
