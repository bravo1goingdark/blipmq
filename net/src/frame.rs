use std::convert::TryFrom;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use thiserror::Error;

pub const LENGTH_FIELD_LEN: usize = 4;
#[allow(dead_code)]
pub const HEADER_FIXED_LEN: usize = LENGTH_FIELD_LEN + 1 + 8;
const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;
// v1: poll-only delivery (HELLO/AUTH/PUBLISH/SUBSCRIBE/ACK/NACK/PING/PONG/POLL).
// v2: server-initiated push delivery via DELIVER frames. v1 clients still
// receive messages via POLL; v2 clients receive DELIVER frames and ignore POLL.
pub const PROTOCOL_VERSION: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Hello = 0x01,
    Auth = 0x02,
    Publish = 0x03,
    Subscribe = 0x04,
    Ack = 0x05,
    Nack = 0x06,
    Ping = 0x07,
    Pong = 0x08,
    Poll = 0x09,
    Deliver = 0x0A,
}

impl From<FrameType> for u8 {
    fn from(t: FrameType) -> Self {
        t as u8
    }
}

impl TryFrom<u8> for FrameType {
    type Error = FrameDecodeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(FrameType::Hello),
            0x02 => Ok(FrameType::Auth),
            0x03 => Ok(FrameType::Publish),
            0x04 => Ok(FrameType::Subscribe),
            0x05 => Ok(FrameType::Ack),
            0x06 => Ok(FrameType::Nack),
            0x07 => Ok(FrameType::Ping),
            0x08 => Ok(FrameType::Pong),
            0x09 => Ok(FrameType::Poll),
            0x0A => Ok(FrameType::Deliver),
            other => Err(FrameDecodeError::UnknownFrameType(other)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub msg_type: FrameType,
    pub correlation_id: u64,
    pub payload: Bytes,
}

#[derive(Debug, Error)]
pub enum FrameDecodeError {
    #[error("invalid frame length: {0}")]
    InvalidLength(u32),

    #[error("frame too large: {0} bytes")]
    FrameTooLarge(u32),

    #[error("unknown frame type: {0}")]
    UnknownFrameType(u8),
}

#[derive(Debug, Error)]
pub enum FrameEncodeError {
    #[error("payload too large: {0} bytes")]
    PayloadTooLarge(usize),
}

/// HELLO payload: [u16 protocol_version]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloPayload {
    pub protocol_version: u16,
}

impl HelloPayload {
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(2);
        buf.put_u16(self.protocol_version);
        buf.freeze()
    }

    pub fn decode(payload: &Bytes) -> Result<Self, FrameDecodeError> {
        if payload.len() != 2 {
            return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
        }
        let mut slice = &payload[..];
        let version = slice.get_u16();
        Ok(Self {
            protocol_version: version,
        })
    }
}

/// AUTH payload: [u16 key_len][key_bytes]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthPayload {
    pub api_key: String,
}

impl AuthPayload {
    pub fn encode(&self) -> Result<Bytes, FrameEncodeError> {
        let key_bytes = self.api_key.as_bytes();
        let key_len = key_bytes.len();
        let key_len_u16 =
            u16::try_from(key_len).map_err(|_| FrameEncodeError::PayloadTooLarge(key_len))?;

        let mut buf = BytesMut::with_capacity(2 + key_len);
        buf.put_u16(key_len_u16);
        buf.put_slice(key_bytes);
        Ok(buf.freeze())
    }

    pub fn decode(payload: &Bytes) -> Result<Self, FrameDecodeError> {
        if payload.len() < 2 {
            return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
        }

        let mut slice = &payload[..];
        let key_len = slice.get_u16() as usize;

        if slice.remaining() != key_len {
            return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
        }

        let key_bytes = slice.copy_to_bytes(key_len);
        let api_key = String::from_utf8(key_bytes.to_vec())
            .map_err(|_| FrameDecodeError::InvalidLength(payload.len() as u32))?;

        Ok(Self { api_key })
    }
}

/// ACK payload: [u64 subscription_id]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckPayload {
    pub subscription_id: u64,
}

impl AckPayload {
    pub fn encode(&self) -> Result<Bytes, FrameEncodeError> {
        let mut buf = BytesMut::with_capacity(8);
        buf.put_u64(self.subscription_id);
        Ok(buf.freeze())
    }

    pub fn decode(payload: &Bytes) -> Result<Self, FrameDecodeError> {
        if payload.len() != 8 {
            return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
        }

        let mut slice = &payload[..];
        let subscription_id = slice.get_u64();
        Ok(Self { subscription_id })
    }
}

/// NACK payload: [u16 code][u16 message_len][message_bytes]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NackPayload {
    pub code: u16,
    pub message: String,
}

impl NackPayload {
    pub fn encode(&self) -> Result<Bytes, FrameEncodeError> {
        let msg_bytes = self.message.as_bytes();
        let msg_len = msg_bytes.len();
        let msg_len_u16 =
            u16::try_from(msg_len).map_err(|_| FrameEncodeError::PayloadTooLarge(msg_len))?;

        let mut buf = BytesMut::with_capacity(2 + 2 + msg_len);
        buf.put_u16(self.code);
        buf.put_u16(msg_len_u16);
        buf.put_slice(msg_bytes);
        Ok(buf.freeze())
    }

    pub fn decode(payload: &Bytes) -> Result<Self, FrameDecodeError> {
        if payload.len() < 4 {
            return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
        }

        let mut slice = &payload[..];
        let code = slice.get_u16();
        let msg_len = slice.get_u16() as usize;

        if slice.remaining() != msg_len {
            return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
        }

        let msg_bytes = slice.copy_to_bytes(msg_len);
        let message = String::from_utf8(msg_bytes.to_vec())
            .map_err(|_| FrameDecodeError::InvalidLength(payload.len() as u32))?;

        Ok(Self { code, message })
    }
}

/// PUBLISH payload: [u8 qos][u16 topic_len][topic_bytes][message_bytes...]
///
/// `topic` and `message` are zero-copy slices of the originating buffer
/// (they share the underlying `Bytes` allocation). Constructing the topic
/// as `Bytes` rather than `String` avoids a per-PUBLISH heap allocation on
/// the broker ingress hot path; UTF-8 is validated in `decode` without
/// copying.
///
/// **Wire format flag bits:** the low 2 bits of `qos` carry the QoS
/// level (0 = at-most-once, 1 = at-least-once). Bit 7 (`0x80`) is the
/// `has_ttl` flag: when set, a `u32 ttl_ms` field follows the qos byte
/// before the topic length. Old clients leave bit 7 clear and rely on
/// the broker's default `message_ttl`; new clients can override
/// per-message. This is wire-compatible with the original v2 format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishPayload {
    pub topic: Bytes,
    pub qos: u8,
    pub message: Bytes,
    /// Optional per-message TTL in milliseconds. When `Some`, overrides
    /// the broker's `BrokerConfig::message_ttl` for this single message.
    pub ttl_ms: Option<u32>,
}

const PUBLISH_FLAG_HAS_TTL: u8 = 0x80;
const PUBLISH_QOS_MASK: u8 = 0x03;

impl PublishPayload {
    pub fn encode(&self) -> Result<Bytes, FrameEncodeError> {
        let topic_len = self.topic.len();
        let topic_len_u16 =
            u16::try_from(topic_len).map_err(|_| FrameEncodeError::PayloadTooLarge(topic_len))?;

        let message_len = self.message.len();
        let qos_byte = if self.ttl_ms.is_some() {
            (self.qos & PUBLISH_QOS_MASK) | PUBLISH_FLAG_HAS_TTL
        } else {
            self.qos & PUBLISH_QOS_MASK
        };
        let extra = if self.ttl_ms.is_some() { 4 } else { 0 };

        let mut buf =
            BytesMut::with_capacity(1 + extra + 2 + topic_len + message_len);
        buf.put_u8(qos_byte);
        if let Some(ttl) = self.ttl_ms {
            buf.put_u32(ttl);
        }
        buf.put_u16(topic_len_u16);
        buf.put_slice(&self.topic);
        buf.put_slice(&self.message);
        Ok(buf.freeze())
    }

    pub fn decode(payload: &Bytes) -> Result<Self, FrameDecodeError> {
        if payload.len() < 3 {
            return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
        }

        let mut slice = &payload[..];
        let qos_byte = slice.get_u8();
        let qos = qos_byte & PUBLISH_QOS_MASK;
        let has_ttl = qos_byte & PUBLISH_FLAG_HAS_TTL != 0;

        let mut header_len = 1usize;
        let ttl_ms = if has_ttl {
            if slice.remaining() < 4 {
                return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
            }
            let ttl = slice.get_u32();
            header_len += 4;
            Some(ttl)
        } else {
            None
        };

        if slice.remaining() < 2 {
            return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
        }
        let topic_len = slice.get_u16() as usize;
        header_len += 2;

        if slice.remaining() < topic_len {
            return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
        }

        // Slice topic out of the parent buffer (refcount bump, no copy).
        let topic = payload.slice(header_len..header_len + topic_len);
        // Validate UTF-8 in place; if invalid, refuse the frame.
        if std::str::from_utf8(&topic).is_err() {
            return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
        }
        let message = payload.slice(header_len + topic_len..);

        Ok(Self {
            topic,
            qos,
            message,
            ttl_ms,
        })
    }

    /// Convenience for callers that want the topic as `&str`. Since `decode`
    /// validates UTF-8, this never panics on a `PublishPayload` produced by
    /// `decode`. For payloads constructed manually, callers must guarantee
    /// the bytes are valid UTF-8.
    #[inline]
    pub fn topic_str(&self) -> &str {
        // SAFETY: invariant maintained by `decode` and by the public API
        // contract (manually-built payloads must use UTF-8 bytes).
        unsafe { std::str::from_utf8_unchecked(&self.topic) }
    }
}

/// DELIVER payload (server-initiated, v2): [u8 qos][u64 delivery_tag][u16 topic_len][topic_bytes][message_bytes...]
///
/// Mirrors `PublishPayload` plus a `delivery_tag`. The tag is 0 for QoS0
/// (fire-and-forget) and the broker's monotonic per-subscription tag for QoS1
/// (used by the client in the subsequent ACK frame).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliverPayload {
    pub qos: u8,
    pub delivery_tag: u64,
    pub topic: String,
    pub message: Bytes,
}

impl DeliverPayload {
    pub fn encode(&self) -> Result<Bytes, FrameEncodeError> {
        let topic_bytes = self.topic.as_bytes();
        let topic_len = topic_bytes.len();
        let topic_len_u16 =
            u16::try_from(topic_len).map_err(|_| FrameEncodeError::PayloadTooLarge(topic_len))?;

        let message_len = self.message.len();
        let mut buf = BytesMut::with_capacity(1 + 8 + 2 + topic_len + message_len);
        buf.put_u8(self.qos);
        buf.put_u64(self.delivery_tag);
        buf.put_u16(topic_len_u16);
        buf.put_slice(topic_bytes);
        buf.put_slice(&self.message);
        Ok(buf.freeze())
    }

    pub fn decode(payload: &Bytes) -> Result<Self, FrameDecodeError> {
        if payload.len() < 1 + 8 + 2 {
            return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
        }

        let mut slice = &payload[..];
        let qos = slice.get_u8();
        let delivery_tag = slice.get_u64();
        let topic_len = slice.get_u16() as usize;

        if slice.remaining() < topic_len {
            return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
        }

        let topic_bytes = slice.copy_to_bytes(topic_len);
        let topic = String::from_utf8(topic_bytes.to_vec())
            .map_err(|_| FrameDecodeError::InvalidLength(payload.len() as u32))?;

        let message = slice.copy_to_bytes(slice.remaining());

        Ok(Self {
            qos,
            delivery_tag,
            topic,
            message,
        })
    }
}

/// SUBSCRIBE payload: [u16 topic_len][topic_bytes][u8 qos]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscribePayload {
    pub topic: String,
    pub qos: u8,
}

impl SubscribePayload {
    pub fn encode(&self) -> Result<Bytes, FrameEncodeError> {
        let topic_bytes = self.topic.as_bytes();
        let topic_len = topic_bytes.len();
        let topic_len_u16 =
            u16::try_from(topic_len).map_err(|_| FrameEncodeError::PayloadTooLarge(topic_len))?;

        let mut buf = BytesMut::with_capacity(2 + topic_len + 1);
        buf.put_u16(topic_len_u16);
        buf.put_slice(topic_bytes);
        buf.put_u8(self.qos);
        Ok(buf.freeze())
    }

    pub fn decode(payload: &Bytes) -> Result<Self, FrameDecodeError> {
        if payload.len() < 3 {
            return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
        }

        let mut slice = &payload[..];
        let topic_len = slice.get_u16() as usize;

        if slice.remaining() < topic_len + 1 {
            return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
        }

        let topic_bytes = slice.copy_to_bytes(topic_len);
        let topic = String::from_utf8(topic_bytes.to_vec())
            .map_err(|_| FrameDecodeError::InvalidLength(payload.len() as u32))?;

        let qos = slice.get_u8();

        Ok(Self { topic, qos })
    }
}

/// POLL payload: [u64 subscription_id]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollPayload {
    pub subscription_id: u64,
}

impl PollPayload {
    pub fn encode(&self) -> Result<Bytes, FrameEncodeError> {
        let mut buf = BytesMut::with_capacity(8);
        buf.put_u64(self.subscription_id);
        Ok(buf.freeze())
    }

    pub fn decode(payload: &Bytes) -> Result<Self, FrameDecodeError> {
        if payload.len() != 8 {
            return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
        }

        let mut slice = &payload[..];
        let subscription_id = slice.get_u64();

        Ok(Self { subscription_id })
    }
}

/// PING and PONG payloads are currently empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PingPayload;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PongPayload;

/// Encode a frame into the provided buffer.
#[inline(always)]
#[tracing::instrument(skip(frame, dst))]
pub fn encode_frame(frame: &Frame, dst: &mut BytesMut) -> Result<(), FrameEncodeError> {
    let payload_len = frame.payload.len();
    let total_len = 1usize
        .checked_add(8)
        .and_then(|v| v.checked_add(payload_len))
        .ok_or(FrameEncodeError::PayloadTooLarge(payload_len))?;

    if total_len > MAX_FRAME_SIZE as usize {
        return Err(FrameEncodeError::PayloadTooLarge(payload_len));
    }

    dst.reserve(LENGTH_FIELD_LEN + total_len);
    dst.put_u32(total_len as u32);
    dst.put_u8(frame.msg_type.into());
    dst.put_u64(frame.correlation_id);
    dst.put_slice(&frame.payload);
    Ok(())
}

/// Encode a complete DELIVER frame (length prefix + header + payload) into
/// `dst` in a single pass, with no intermediate `Bytes` allocation. This is
/// the writer task's hot path; avoiding the allocate→freeze→copy pattern
/// from `DeliverPayload::encode` + `encode_frame` matters at multi-million
/// msg/s.
///
/// Wire format mirrors `DeliverPayload`:
///   [u32 frame_len][u8 msg_type=Deliver][u64 correlation_id]
///     [u8 qos][u64 delivery_tag][u16 topic_len][topic_bytes][message_bytes]
///
/// `correlation_id` is set to `delivery_tag` so QoS1 ACKs from the client
/// can identify which delivery they're acking.
#[inline(always)]
pub fn encode_deliver_frame(
    dst: &mut BytesMut,
    qos: u8,
    delivery_tag: u64,
    topic: &str,
    message: &[u8],
) -> Result<(), FrameEncodeError> {
    let topic_bytes = topic.as_bytes();
    let topic_len = topic_bytes.len();
    let topic_len_u16 =
        u16::try_from(topic_len).map_err(|_| FrameEncodeError::PayloadTooLarge(topic_len))?;

    let message_len = message.len();

    // Frame body = msg_type(1) + correlation_id(8) + qos(1) + delivery_tag(8)
    //              + topic_len(2) + topic_bytes + message_bytes
    //            = 20 + topic_len + message_len
    let frame_body_len: usize = 20usize
        .checked_add(topic_len)
        .and_then(|v| v.checked_add(message_len))
        .ok_or(FrameEncodeError::PayloadTooLarge(message_len))?;

    if frame_body_len > MAX_FRAME_SIZE as usize {
        return Err(FrameEncodeError::PayloadTooLarge(frame_body_len));
    }

    dst.reserve(LENGTH_FIELD_LEN + frame_body_len);
    dst.put_u32(frame_body_len as u32);
    dst.put_u8(FrameType::Deliver.into());
    dst.put_u64(delivery_tag); // correlation_id
    dst.put_u8(qos);
    dst.put_u64(delivery_tag); // payload tag (mirror; client also reads from frame correlation_id)
    dst.put_u16(topic_len_u16);
    dst.put_slice(topic_bytes);
    dst.put_slice(message);
    Ok(())
}

/// Try to decode a single frame from the buffer.
///
/// Returns `Ok(None)` if there is not yet enough data to decode a full frame.
#[inline(always)]
#[tracing::instrument(skip(src))]
pub fn try_decode_frame(src: &mut BytesMut) -> Result<Option<Frame>, FrameDecodeError> {
    if src.len() < LENGTH_FIELD_LEN {
        return Ok(None);
    }

    let mut length_bytes = &src[..LENGTH_FIELD_LEN];
    let frame_len = length_bytes.get_u32();

    if frame_len == 0 {
        return Err(FrameDecodeError::InvalidLength(frame_len));
    }

    if frame_len > MAX_FRAME_SIZE {
        return Err(FrameDecodeError::FrameTooLarge(frame_len));
    }

    let frame_len_usize = frame_len as usize;
    if frame_len_usize < 1 + 8 {
        return Err(FrameDecodeError::InvalidLength(frame_len));
    }

    if src.len() < LENGTH_FIELD_LEN + frame_len_usize {
        return Ok(None);
    }

    let total = LENGTH_FIELD_LEN + frame_len_usize;
    let mut frame_bytes = src.split_to(total);
    frame_bytes.advance(LENGTH_FIELD_LEN);

    let msg_type_raw = frame_bytes.get_u8();
    let msg_type = FrameType::try_from(msg_type_raw)?;
    let correlation_id = frame_bytes.get_u64();
    // Zero-copy: hand the remaining BytesMut tail to the caller as immutable
    // Bytes via freeze() instead of copying with copy_to_bytes.
    let payload = frame_bytes.freeze();

    Ok(Some(Frame {
        msg_type,
        correlation_id,
        payload,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip_single() {
        let frame = Frame {
            msg_type: FrameType::Hello,
            correlation_id: 42,
            payload: Bytes::from_static(b"hello world"),
        };

        let mut buf = BytesMut::new();
        encode_frame(&frame, &mut buf).unwrap();

        let decoded = try_decode_frame(&mut buf)
            .unwrap()
            .expect("expected one complete frame");

        assert_eq!(decoded, frame);
        assert!(try_decode_frame(&mut buf).unwrap().is_none());
    }

    #[test]
    fn encode_decode_roundtrip_pipelined() {
        let frame1 = Frame {
            msg_type: FrameType::Ping,
            correlation_id: 1,
            payload: Bytes::from_static(b"first"),
        };
        let frame2 = Frame {
            msg_type: FrameType::Pong,
            correlation_id: 2,
            payload: Bytes::from_static(b"second"),
        };

        let mut buf = BytesMut::new();
        encode_frame(&frame1, &mut buf).unwrap();
        encode_frame(&frame2, &mut buf).unwrap();

        let decoded1 = try_decode_frame(&mut buf)
            .unwrap()
            .expect("expected first frame");
        let decoded2 = try_decode_frame(&mut buf)
            .unwrap()
            .expect("expected second frame");

        assert_eq!(decoded1, frame1);
        assert_eq!(decoded2, frame2);
        assert!(try_decode_frame(&mut buf).unwrap().is_none());
    }

    #[test]
    fn incomplete_buffer_returns_none() {
        let frame = Frame {
            msg_type: FrameType::Ack,
            correlation_id: 99,
            payload: Bytes::from_static(b"short"),
        };

        let mut full = BytesMut::new();
        encode_frame(&frame, &mut full).unwrap();

        // Take only part of the encoded frame.
        let mut partial = full.split_to(3);
        assert!(try_decode_frame(&mut partial).unwrap().is_none());
    }

    #[test]
    fn publish_payload_with_ttl_roundtrip() {
        let original = PublishPayload {
            topic: Bytes::from_static(b"orders.us.created"),
            qos: 1,
            message: Bytes::from_static(b"hello"),
            ttl_ms: Some(5_000),
        };
        let encoded = original.encode().expect("encode");
        let decoded = PublishPayload::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn publish_payload_without_ttl_roundtrip() {
        // No TTL: wire format should be byte-compatible with the
        // pre-TTL v2 layout.
        let original = PublishPayload {
            topic: Bytes::from_static(b"a.b.c"),
            qos: 0,
            message: Bytes::from_static(b"x"),
            ttl_ms: None,
        };
        let encoded = original.encode().expect("encode");
        // Layout: [qos:1][topic_len:2][topic][message]
        assert_eq!(encoded.len(), 1 + 2 + 5 + 1);
        assert_eq!(encoded[0] & 0x80, 0, "has_ttl flag must be clear");
        let decoded = PublishPayload::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn deliver_payload_roundtrip() {
        let original = DeliverPayload {
            qos: 1,
            delivery_tag: 0xDEAD_BEEF_CAFE_F00D,
            topic: "orders.us.created".to_string(),
            message: Bytes::from_static(b"hello, deliver"),
        };

        let encoded = original.encode().expect("encode");
        // Sanity: 5-byte fixed prefix [qos:1][tag:8][topic_len:2] before topic+message.
        assert_eq!(encoded.len(), 1 + 8 + 2 + original.topic.len() + original.message.len());

        let decoded = DeliverPayload::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn protocol_version_is_v2() {
        assert_eq!(PROTOCOL_VERSION, 2);
    }

    #[test]
    fn deliver_frame_type_decodes() {
        let raw: u8 = FrameType::Deliver.into();
        assert_eq!(raw, 0x0A);
        assert_eq!(FrameType::try_from(0x0A).unwrap(), FrameType::Deliver);
    }
}
