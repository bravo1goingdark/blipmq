use prost::Message as ProstMessage;
use std::io::Cursor;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/blipmq.rs"));
}

pub use proto::{client_command::Action, ClientCommand};

//// Convenience constructor for a publish command.
pub fn new_pub(topic: impl Into<String>, payload: impl Into<Vec<u8>>) -> ClientCommand {
    new_pub_with_ttl(topic, payload, 0)
}

/// Convenience constructor for a publish command with TTL.
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

/// Convenience constructor for a subscribe command.
pub fn new_sub(topic: impl Into<String>) -> ClientCommand {
    ClientCommand {
        action: Action::Sub as i32,
        topic: topic.into(),
        payload: Vec::new(),
        ttl_ms : 0,
    }
}

/// Convenience constructor for an unsubscribe command.
pub fn new_unsub(topic: impl Into<String>) -> ClientCommand {
    ClientCommand {
        action: Action::Unsub as i32,
        topic: topic.into(),
        payload: Vec::new(),
        ttl_ms : 0,
    }
}

/// Convenience constructor for a quit command.
pub fn new_quit() -> ClientCommand {
    ClientCommand {
        action: Action::Quit as i32,
        topic: String::new(),
        payload: Vec::new(),
        ttl_ms : 0,
    }
}

/// Serialize a command to bytes with no length prefix.
pub fn encode_command(cmd: &ClientCommand) -> Vec<u8> {
    let mut buf = Vec::with_capacity(cmd.encoded_len());
    cmd.encode(&mut buf).expect("encode command");
    buf
}

/// Deserialize a command from bytes.
pub fn decode_command(bytes: &[u8]) -> Result<ClientCommand, prost::DecodeError> {
    ClientCommand::decode(Cursor::new(bytes))
}
