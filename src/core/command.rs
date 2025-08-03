mod generated {
    include!(concat!(env!("OUT_DIR"), "/flatbuffers/mod.rs"));
}

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
    pub payload: Vec<u8>,
    pub ttl_ms: u64,
}

pub fn new_pub(topic: impl Into<String>, payload: impl Into<Vec<u8>>) -> ClientCommand {
    new_pub_with_ttl(topic, payload, 0)
}

pub fn new_pub_with_ttl(
    topic: impl Into<String>,
    payload: impl Into<Vec<u8>>,
    ttl_ms: u64,
) -> ClientCommand {
    ClientCommand {
        action: Action::Pub,
        topic: topic.into(),
        payload: payload.into(),
        ttl_ms,
    }
}

pub fn new_sub(topic: impl Into<String>) -> ClientCommand {
    ClientCommand {
        action: Action::Sub,
        topic: topic.into(),
        payload: Vec::new(),
        ttl_ms: 0,
    }
}

pub fn new_unsub(topic: impl Into<String>) -> ClientCommand {
    ClientCommand {
        action: Action::Unsub,
        topic: topic.into(),
        payload: Vec::new(),
        ttl_ms: 0,
    }
}

pub fn new_quit() -> ClientCommand {
    ClientCommand {
        action: Action::Quit,
        topic: String::new(),
        payload: Vec::new(),
        ttl_ms: 0,
    }
}

/// Serialize a command to raw bytes (no length prefix).
pub fn encode_command(cmd: &ClientCommand) -> Vec<u8> {
    let mut builder = FlatBufferBuilder::new();
    let topic = builder.create_string(&cmd.topic);
    let payload = builder.create_vector(&cmd.payload);
    let args = blipmq::ClientCommandArgs {
        action: cmd.action.into(),
        topic: Some(topic),
        payload: Some(payload),
        ttl_ms: cmd.ttl_ms,
    };
    let offset = blipmq::ClientCommand::create(&mut builder, &args);
    builder.finish(offset, None);
    builder.finished_data().to_vec()
}

/// Serialize a command to length-prefixed Protobuf frame.
pub fn encode_command_with_len_prefix(cmd: &ClientCommand) -> Vec<u8> {
    let data = encode_command(cmd);
    let mut buf = Vec::with_capacity(4 + data.len());
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(&data);
    buf
}

pub fn decode_command(bytes: &[u8]) -> Result<ClientCommand, InvalidFlatbuffer> {
    let c = root::<blipmq::ClientCommand>(bytes)?;
    Ok(ClientCommand {
        action: c.action().into(),
        topic: c.topic().unwrap_or("").to_string(),
        payload: c
            .payload()
            .map(|v| v.iter().collect::<Vec<_>>())
            .unwrap_or_default(),
        ttl_ms: c.ttl_ms(),
    })
}
