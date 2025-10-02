//! Ultra-fast protocol parser
//! 
//! Binary protocol format:
//! [1 byte op] [2 bytes topic_len] [1 byte flags] [topic] [payload]
//! 
//! Operations:
//! - 0x01: CONNECT
//! - 0x02: PUBLISH  
//! - 0x03: SUBSCRIBE
//! - 0x04: UNSUBSCRIBE
//! - 0x05: PING
//! - 0x06: PONG
//! - 0x07: DISCONNECT

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crossbeam::channel::{Receiver, Sender};
use bytes::Bytes;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OpCode {
    Connect = 0x01,
    Publish = 0x02,
    Subscribe = 0x03,
    Unsubscribe = 0x04,
    Ping = 0x05,
    Pong = 0x06,
    Disconnect = 0x07,
}

impl OpCode {
    #[inline(always)]
    fn from_u8(val: u8) -> Option<Self> {
        match val {
            0x01 => Some(OpCode::Connect),
            0x02 => Some(OpCode::Publish),
            0x03 => Some(OpCode::Subscribe),
            0x04 => Some(OpCode::Unsubscribe),
            0x05 => Some(OpCode::Ping),
            0x06 => Some(OpCode::Pong),
            0x07 => Some(OpCode::Disconnect),
            _ => None,
        }
    }
}

/// Parsed message ready for routing
#[derive(Debug)]
pub struct ParsedMessage {
    pub conn_id: usize,
    pub op: OpCode,
    pub topic: Bytes,
    pub payload: Bytes,
    pub flags: u8,
    pub timestamp: u64,
}

/// Parser thread main loop
pub fn parser_thread_loop(
    thread_id: usize,
    rx: Receiver<super::network::RawMessage>,
    tx: Sender<super::routing::ParsedMessage>,
    shutdown: Arc<AtomicBool>
) {
    println!("🔍 Parser thread {} starting", thread_id);
    
    let mut parse_count = 0u64;
    let mut error_count = 0u64;
    
    while !shutdown.load(Ordering::Relaxed) {
        // Receive raw message
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(raw) => {
                // Parse message (zero-copy)
                match parse_message(&raw.data) {
                    Ok((op, topic, payload, flags)) => {
                        let parsed = ParsedMessage {
                            conn_id: raw.conn_id,
                            op,
                            topic,
                            payload,
                            flags,
                            timestamp: raw.timestamp,
                        };
                        
                        // Send to routing
                        if tx.send(parsed).is_err() {
                            eprintln!("Failed to send to routing");
                        }
                        
                        parse_count += 1;
                    }
                    Err(e) => {
                        eprintln!("Parse error: {}", e);
                        error_count += 1;
                    }
                }
            }
            Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                // No message, continue
            }
            Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
        
        // Log stats periodically
        if parse_count % 100_000 == 0 && parse_count > 0 {
            println!("Parser {}: {} messages, {} errors", thread_id, parse_count, error_count);
        }
    }
    
    println!("🔍 Parser thread {} shutting down (parsed: {}, errors: {})", 
             thread_id, parse_count, error_count);
}

/// Parse a message (zero-copy)
#[inline(always)]
fn parse_message(data: &Bytes) -> Result<(OpCode, Bytes, Bytes, u8), String> {
    if data.len() < 4 {
        return Err("Message too short".to_string());
    }
    
    // Parse header
    let op_byte = data[0];
    let topic_len = u16::from_be_bytes([data[1], data[2]]) as usize;
    let flags = data[3];
    
    // Parse opcode
    let op = OpCode::from_u8(op_byte)
        .ok_or_else(|| format!("Invalid opcode: {}", op_byte))?;
    
    // Validate length
    if data.len() < 4 + topic_len {
        return Err("Invalid topic length".to_string());
    }
    
    // Extract topic and payload (zero-copy slices)
    let topic = data.slice(4..4 + topic_len);
    let payload = if data.len() > 4 + topic_len {
        data.slice(4 + topic_len..)
    } else {
        Bytes::new()
    };
    
    Ok((op, topic, payload, flags))
}

// Re-export for routing module
pub use ParsedMessage as ParsedMsg;

// Add MessageType enum for server compatibility
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MessageType {
    Publish,
    Subscribe,
    Unsubscribe,
    Ping,
    Pong,
    Ack,
}

impl ParsedMessage {
    pub fn msg_type(&self) -> MessageType {
        match self.op {
            OpCode::Publish => MessageType::Publish,
            OpCode::Subscribe => MessageType::Subscribe,
            OpCode::Unsubscribe => MessageType::Unsubscribe,
            OpCode::Ping => MessageType::Ping,
            OpCode::Pong => MessageType::Pong,
            _ => MessageType::Ack,
        }
    }
}

// Add ProtocolParser for server
use bytes::BytesMut;

pub struct ProtocolParser {
    buffer: BytesMut,
}

impl ProtocolParser {
    pub fn new() -> Self {
        Self {
            buffer: BytesMut::with_capacity(65536),
        }
    }
    
    pub fn parse(&mut self, data: &Bytes) -> Result<Vec<ParsedMessage>, String> {
        let mut messages = Vec::new();
        
        // Simple parser - assumes one complete message per data chunk
        match parse_message(data) {
            Ok((op, topic_bytes, payload, flags)) => {
                let msg = ParsedMessage {
                    conn_id: 0, // Will be set by caller
                    op,
                    topic: topic_bytes.clone(),
                    payload,
                    flags,
                    timestamp: 0,
                };
                messages.push(msg);
            }
            Err(e) => return Err(e),
        }
        
        Ok(messages)
    }
}
