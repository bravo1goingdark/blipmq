use prost::Message as ProstMessage;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/blipmq.rs"));
}

pub use proto::{client_command::Action, ClientCommand};

/// Create a publish command (no TTL).
pub fn new_pub(topic: impl Into<String>, payload: impl Into<Vec<u8>>) -> ClientCommand {
    new_pub_with_ttl(topic, payload, 0)
}

/// Create a publish command with TTL.
pub fn new_pub_with_ttl(
    topic: impl Into<String>,
    payload: impl Into<Vec<u8>>,
    ttl_ms: u64,
) -> ClientCommand {
    ClientCommand {
        action: Action::Pub as i32,
        topic: topic.into(),
        payload: payload.into(),
        ttl_ms,
    }
}

/// Create a subscribe command.
pub fn new_sub(topic: impl Into<String>) -> ClientCommand {
    ClientCommand {
        action: Action::Sub as i32,
        topic: topic.into(),
        payload: Vec::new(),
        ttl_ms: 0,
    }
}

/// Create an unsubscribe command.
pub fn new_unsub(topic: impl Into<String>) -> ClientCommand {
    ClientCommand {
        action: Action::Unsub as i32,
        topic: topic.into(),
        payload: Vec::new(),
        ttl_ms: 0,
    }
}

/// Create a quit command.
pub fn new_quit() -> ClientCommand {
    ClientCommand {
        action: Action::Quit as i32,
        topic: String::new(),
        payload: Vec::new(),
        ttl_ms: 0,
    }
}

/// Serialize a command to raw bytes (no length prefix).
pub fn encode_command(cmd: &ClientCommand) -> Vec<u8> {
    let mut buf = Vec::with_capacity(cmd.encoded_len());
    cmd.encode(&mut buf).expect("encode command");
    buf
}

/// Serialize a command to length-prefixed Protobuf frame.
pub fn encode_command_with_len_prefix(cmd: &ClientCommand) -> Vec<u8> {
    let len = cmd.encoded_len();
    let mut buf = Vec::with_capacity(len + 4);
    buf.extend_from_slice(&(len as u32).to_be_bytes());
    cmd.encode(&mut buf).expect("encode command");
    buf
}

/// Deserialize a command from raw bytes.
pub fn decode_command(bytes: &[u8]) -> Result<ClientCommand, prost::DecodeError> {
    ClientCommand::decode(bytes)
}
