//! Ultra-fast binary protocol for BlipMQ
//! 
//! Protocol design for minimal latency:
//! - Fixed-size headers for O(1) parsing
//! - Zero-copy wherever possible
//! - Stack allocation for small messages
//! - No heap allocations in hot path

use bytes::{Bytes, BytesMut, BufMut};
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

/// Protocol version
const PROTOCOL_VERSION: u8 = 2;

/// Maximum topic length (for stack allocation)
const MAX_TOPIC_LEN: usize = 255;

/// Command types (4 bits)
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpCode {
    Pub = 0x01,
    Sub = 0x02,
    Unsub = 0x03,
    Quit = 0x04,
    Ack = 0x05,
    Msg = 0x06,
    Ping = 0x07,
    Pong = 0x08,
}

/// Fixed-size frame header (8 bytes)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct FrameHeader {
    /// Total frame size including header
    pub frame_size: u32,
    /// Operation code (4 bits) | Flags (4 bits)
    pub op_flags: u8,
    /// Topic length
    pub topic_len: u8,
    /// Reserved for alignment
    pub reserved: u16,
}

impl FrameHeader {
    #[inline(always)]
    pub const fn size() -> usize {
        mem::size_of::<Self>()
    }

    #[inline(always)]
    pub fn opcode(&self) -> OpCode {
        unsafe { mem::transmute((self.op_flags >> 4) & 0x0F) }
    }

    #[inline(always)]
    pub fn flags(&self) -> u8 {
        self.op_flags & 0x0F
    }

    #[inline(always)]
    pub fn set_opcode(&mut self, op: OpCode) {
        self.op_flags = ((op as u8) << 4) | (self.op_flags & 0x0F);
    }

    #[inline(always)]
    pub fn set_flags(&mut self, flags: u8) {
        self.op_flags = (self.op_flags & 0xF0) | (flags & 0x0F);
    }
}

/// Zero-copy frame for reading
pub struct Frame<'a> {
    pub header: FrameHeader,
    pub topic: &'a [u8],
    pub payload: &'a [u8],
}

impl<'a> Frame<'a> {
    /// Parse frame from bytes without allocation
    #[inline(always)]
    pub fn parse(data: &'a [u8]) -> Result<Self, &'static str> {
        if data.len() < FrameHeader::size() {
            return Err("insufficient data for header");
        }

        // Read header directly (zero-copy)
        let header = unsafe {
            ptr::read_unaligned(data.as_ptr() as *const FrameHeader)
        };

        if data.len() < header.frame_size as usize {
            return Err("incomplete frame");
        }

        let mut offset = FrameHeader::size();
        
        // Extract topic (zero-copy slice)
        let topic_end = offset + header.topic_len as usize;
        if topic_end > data.len() {
            return Err("invalid topic length");
        }
        let topic = &data[offset..topic_end];
        offset = topic_end;

        // Extract payload (zero-copy slice)
        let payload = &data[offset..header.frame_size as usize];

        Ok(Frame {
            header,
            topic,
            payload,
        })
    }

    #[inline(always)]
    pub fn topic_str(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(self.topic)
    }
}

/// Frame builder for writing (stack-allocated for small messages)
pub struct FrameBuilder {
    buffer: BytesMut,
}

/// Global message ID counter for fast ID generation
static NEXT_MSG_ID: AtomicU64 = AtomicU64::new(1);

/// Fast message ID generation
#[inline(always)]
pub fn generate_message_id() -> u64 {
    NEXT_MSG_ID.fetch_add(1, Ordering::Relaxed)
}

/// Fast timestamp generation using high-resolution clock
#[inline(always)]
pub fn fast_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

impl FrameBuilder {
    #[inline(always)]
    pub fn new() -> Self {
        Self {
            buffer: BytesMut::with_capacity(1024),
        }
    }

    #[inline(always)]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buffer: BytesMut::with_capacity(cap),
        }
    }

    /// Build a publish frame
    #[inline(always)]
    pub fn publish(&mut self, topic: &[u8], payload: &[u8], ttl_ms: u32) -> Bytes {
        self.buffer.clear();
        
        let frame_size = FrameHeader::size() + topic.len() + payload.len() + 4; // +4 for TTL
        
        // Write header
        let header = FrameHeader {
            frame_size: frame_size as u32,
            op_flags: (OpCode::Pub as u8) << 4,
            topic_len: topic.len() as u8,
            reserved: 0,
        };
        
        self.buffer.put_slice(unsafe {
            std::slice::from_raw_parts(
                &header as *const _ as *const u8,
                FrameHeader::size()
            )
        });
        
        // Write topic
        self.buffer.put_slice(topic);
        
        // Write TTL
        self.buffer.put_u32_le(ttl_ms);
        
        // Write payload
        self.buffer.put_slice(payload);
        
        self.buffer.split().freeze()
    }

    /// Build a subscribe frame
    #[inline(always)]
    pub fn subscribe(&mut self, topic: &[u8]) -> Bytes {
        self.buffer.clear();
        
        let frame_size = FrameHeader::size() + topic.len();
        
        let header = FrameHeader {
            frame_size: frame_size as u32,
            op_flags: (OpCode::Sub as u8) << 4,
            topic_len: topic.len() as u8,
            reserved: 0,
        };
        
        self.buffer.put_slice(unsafe {
            std::slice::from_raw_parts(
                &header as *const _ as *const u8,
                FrameHeader::size()
            )
        });
        
        self.buffer.put_slice(topic);
        
        self.buffer.split().freeze()
    }

    /// Build a message frame for delivery
    #[inline(always)]
    pub fn message(&mut self, topic: &[u8], payload: &[u8], msg_id: u64) -> Bytes {
        self.buffer.clear();
        
        let frame_size = FrameHeader::size() + topic.len() + 8 + payload.len(); // +8 for msg_id
        
        let header = FrameHeader {
            frame_size: frame_size as u32,
            op_flags: (OpCode::Msg as u8) << 4,
            topic_len: topic.len() as u8,
            reserved: 0,
        };
        
        self.buffer.put_slice(unsafe {
            std::slice::from_raw_parts(
                &header as *const _ as *const u8,
                FrameHeader::size()
            )
        });
        
        self.buffer.put_slice(topic);
        self.buffer.put_u64_le(msg_id);
        self.buffer.put_slice(payload);
        
        self.buffer.split().freeze()
    }

    /// Build acknowledgment frame
    #[inline(always)]
    pub fn ack(&mut self, topic: &[u8]) -> Bytes {
        self.buffer.clear();
        
        let frame_size = FrameHeader::size() + topic.len();
        
        let header = FrameHeader {
            frame_size: frame_size as u32,
            op_flags: (OpCode::Ack as u8) << 4,
            topic_len: topic.len() as u8,
            reserved: 0,
        };
        
        self.buffer.put_slice(unsafe {
            std::slice::from_raw_parts(
                &header as *const _ as *const u8,
                FrameHeader::size()
            )
        });
        
        self.buffer.put_slice(topic);
        
        self.buffer.split().freeze()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_roundtrip() {
        let mut builder = FrameBuilder::new();
        let frame_bytes = builder.publish(b"test/topic", b"hello world", 60000);
        
        let parsed = Frame::parse(&frame_bytes).unwrap();
        assert_eq!(parsed.header.opcode(), OpCode::Pub);
        assert_eq!(parsed.topic, b"test/topic");
        assert_eq!(&parsed.payload[4..], b"hello world"); // Skip TTL
    }

    #[test]
    fn test_header_size() {
        assert_eq!(FrameHeader::size(), 8);
    }
}
