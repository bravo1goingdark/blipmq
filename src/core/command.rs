mod generated {
    include!(concat!(env!("OUT_DIR"), "/flatbuffers/mod.rs"));
}

use bytes::{BufMut, Bytes, BytesMut};
use flatbuffers::{root, FlatBufferBuilder, InvalidFlatbuffer};
use generated::blipmq;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Pub,
    Sub,
    Unsub,
    Quit,
}

impl From<Action> for blipmq::Action {
    fn from(a: Action) -> Self {
        match a {
            Action::Pub => blipmq::Action::PUB,
            Action::Sub => blipmq::Action::SUB,
            Action::Unsub => blipmq::Action::UNSUB,
            Action::Quit => blipmq::Action::QUIT,
        }
    }
}

impl From<blipmq::Action> for Action {
    fn from(a: blipmq::Action) -> Self {
        match a {
            blipmq::Action::SUB => Action::Sub,
            blipmq::Action::UNSUB => Action::Unsub,
            blipmq::Action::QUIT => Action::Quit,
            _ => Action::Pub,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClientCommand {
    pub action: Action,
    pub topic: String,
    pub payload: Bytes, // zero-copy friendly
    pub ttl_ms: u64,
}

#[inline]
pub fn new_pub(topic: impl Into<String>, payload: impl Into<Bytes>) -> ClientCommand {
    new_pub_with_ttl(topic, payload, 0)
}

#[inline]
pub fn new_pub_with_ttl(
    topic: impl Into<String>,
    payload: impl Into<Bytes>,
    ttl_ms: u64,
) -> ClientCommand {
    ClientCommand {
        action: Action::Pub,
        topic: topic.into(),
        payload: payload.into(),
        ttl_ms,
    }
}

#[inline]
pub fn new_sub(topic: impl Into<String>) -> ClientCommand {
    ClientCommand {
        action: Action::Sub,
        topic: topic.into(),
        payload: Bytes::new(),
        ttl_ms: 0,
    }
}

#[inline]
pub fn new_unsub(topic: impl Into<String>) -> ClientCommand {
    ClientCommand {
        action: Action::Unsub,
        topic: topic.into(),
        payload: Bytes::new(),
        ttl_ms: 0,
    }
}

#[inline]
pub fn new_quit() -> ClientCommand {
    ClientCommand {
        action: Action::Quit,
        topic: String::new(),
        payload: Bytes::new(),
        ttl_ms: 0,
    }
}

/* ------------------------------- Encode --------------------------------- */

/// Serialize a command to raw bytes (no length prefix).
#[inline]
pub fn encode_command(cmd: &ClientCommand) -> Vec<u8> {
    // Rough capacity hint: topic + payload + small header.
    let mut builder = FlatBufferBuilder::with_capacity(16 + cmd.topic.len() + cmd.payload.len());

    let topic = builder.create_string(&cmd.topic);
    // Only set `payload` for PUB with non-empty data to save bytes on SUB/UNSUB/QUIT.
    let payload_fb = if cmd.payload.is_empty() {
        None
    } else {
        Some(builder.create_vector(cmd.payload.as_ref()))
    };

    let args = blipmq::ClientCommandArgs {
        action: cmd.action.into(),
        topic: Some(topic),
        payload: payload_fb,
        ttl_ms: cmd.ttl_ms,
    };
    let offset = blipmq::ClientCommand::create(&mut builder, &args);
    builder.finish(offset, None);
    builder.finished_data().to_vec()
}

/// Serialize a command to a **length-prefixed FlatBuffer** frame.
#[inline]
pub fn encode_command_with_len_prefix(cmd: &ClientCommand) -> Vec<u8> {
    let data = encode_command(cmd);
    let mut buf = Vec::with_capacity(4 + data.len());
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(&data);
    buf
}

/// Zero-copy friendly: write a length-prefixed command directly into `buf`.
#[inline]
pub fn encode_command_into(cmd: &ClientCommand, buf: &mut BytesMut) {
    let mut builder = FlatBufferBuilder::with_capacity(16 + cmd.topic.len() + cmd.payload.len());

    let topic = builder.create_string(&cmd.topic);
    let payload_fb = if cmd.payload.is_empty() {
        None
    } else {
        Some(builder.create_vector(cmd.payload.as_ref()))
    };

    let args = blipmq::ClientCommandArgs {
        action: cmd.action.into(),
        topic: Some(topic),
        payload: payload_fb,
        ttl_ms: cmd.ttl_ms,
    };
    let offset = blipmq::ClientCommand::create(&mut builder, &args);
    builder.finish(offset, None);

    let data = builder.finished_data();
    buf.reserve(4 + data.len());
    buf.put_u32(data.len() as u32);
    buf.extend_from_slice(data);
}

/* ------------------------------- Decode --------------------------------- */

#[inline]
pub fn decode_command(bytes: &[u8]) -> Result<ClientCommand, InvalidFlatbuffer> {
    let c = root::<blipmq::ClientCommand>(bytes)?;

    // Copy out payload to an owned Bytes; we cannot borrow `bytes` beyond this call.
    let payload = if let Some(v) = c.payload() {
        // FlatBuffers Vector<u8> → &[u8]
        Bytes::copy_from_slice(v.bytes())
    } else {
        Bytes::new()
    };

    Ok(ClientCommand {
        action: c.action().into(),
        topic: c.topic().unwrap_or("").to_string(),
        payload,
        ttl_ms: c.ttl_ms(),
    })
}
