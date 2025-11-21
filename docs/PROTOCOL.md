# BlipMQ Binary Protocol

This document describes the BlipMQ TCP wire protocol implemented by `blipmq_net`.

All communication happens over a single, long-lived TCP connection per client. Frames are length-prefixed and support pipelining.

## Frame Layout

Each frame has the following binary structure:

```text
+-----------------+-------------------+---------------------+--------------+
| u32 length      | u8 frame_type     | u64 correlation_id  | payload ...  |
+-----------------+-------------------+---------------------+--------------+
   4 bytes (BE)       1 byte               8 bytes (BE)        length-9
```

- `length`:
  - 32-bit **big-endian** unsigned integer.
  - Number of bytes following the length field:
    - `1 (type) + 8 (correlation_id) + payload_len`.
  - Must be ≥ `1 + 8` and ≤ `16 * 1024 * 1024` (16 MiB).
- `frame_type`:
  - One-byte discriminant (`FrameType`).
- `correlation_id`:
  - 64-bit **big-endian** integer.
  - Chosen by the client for requests.
  - Echoed by the server in responses or used as a delivery tag for QoS1.
- `payload`:
  - Type-specific binary payload (may be empty).

If `length == 0`, `length < 9`, or `length > MAX_FRAME_SIZE`, frame decoding fails and the connection is closed by the server.

## Frame Types

`FrameType` is a `u8` enum:

| Name       | Value | Direction       | Description                               |
|-----------|-------|-----------------|-------------------------------------------|
| HELLO     | 0x01  | Client → Server | Protocol version negotiation              |
| AUTH      | 0x02  | Client → Server | API key authentication                    |
| PUBLISH   | 0x03  | Both            | Publish or deliver a message              |
| SUBSCRIBE | 0x04  | Client → Server | Subscribe to a topic                      |
| ACK       | 0x05  | Both            | ACK for handshake or QoS1 deliveries      |
| NACK      | 0x06  | Server → Client | Error response                            |
| PING      | 0x07  | Client → Server | Liveness check                            |
| PONG      | 0x08  | Server → Client | Liveness response                         |
| POLL      | 0x09  | Client → Server | Request next message for a subscription   |

Unknown frame types result in a decode error and connection close.

## HELLO / AUTH Handshake

### HELLO

Client → Server.

Payload:

```text
[u16 protocol_version]
```

- `protocol_version`:
  - Current value is `1`.

Server behavior:

- If version matches:
  - Responds with `ACK(subscription_id = 0)`.
- If version mismatches:
  - Responds with `NACK(code = 426, message = "unsupported protocol version")`.

### AUTH

Client → Server.

Payload:

```text
[u16 key_len][key_bytes...]
```

- `key_len`:
  - Length of the UTF-8 API key in bytes (`≤ u16::MAX`).
- `key_bytes`:
  - UTF-8 encoded API key.

Server behavior:

- If HELLO has not been performed:
  - `NACK(400, "HELLO not performed")`.
- If already authenticated:
  - `NACK(400, "already authenticated")`.
- If payload malformed:
  - `NACK(400, "invalid AUTH payload")`.
- If API key invalid:
  - `NACK(401, "invalid API key")`.
- On success:
  - `ACK(subscription_id = 0)`.

After successful HELLO+AUTH:

- Client may send PUBLISH, SUBSCRIBE, POLL, and PING.
- Non-HELLO/AUTH frames before auth complete yield `NACK(401, "unauthenticated")`.

## Payload Formats

All multi-byte integers are big-endian.

### PUBLISH

Client → Server (publish).
Server → Client (delivery).

Payload:

```text
[u8 qos][u16 topic_len][topic_bytes...][message_bytes...]
```

- `qos`:
  - `0` → QoS0 (AtMostOnce).
  - `1` → QoS1 (AtLeastOnce).
- `topic_len`:
  - Length of topic in bytes (`u16`).
- `topic_bytes`:
  - UTF-8 topic name.
- `message_bytes`:
  - Opaque payload.

Server behavior on incoming PUBLISH:

- If topic empty:
  - `NACK(400, "empty topic")`.
- If qos not in {0,1}:
  - `NACK(400, "invalid QoS value")`.
- If QoS1 and WAL configured:
  - Append to WAL.
  - On WAL error: `NACK(500, "durable publish failed: ...")`.
  - On success: enqueue message to subscriber queues.
- Otherwise:
  - Enqueue message in-memory only.
- On success:
  - No response frame (fire-and-forget).

Server behavior when sending PUBLISH as delivery:

- Sent in response to a POLL request.
- `qos` and topic/payload fields reflect the message.
- `correlation_id`:
  - For QoS1: equals `DeliveryTag.value()` to be used in ACK.
  - For QoS0: usually matches the POLL correlation id.

### SUBSCRIBE

Client → Server.

Payload:

```text
[u16 topic_len][topic_bytes...][u8 qos]
```

- `topic_len`:
  - Length of UTF-8 topic.
- `topic_bytes`:
  - Topic name.
- `qos`:
  - Requested QoS (`0` or `1`).

Server behavior:

- Invalid payload:
  - `NACK(400, "invalid SUBSCRIBE payload")`.
- Empty topic:
  - `NACK(400, "empty topic")`.
- Invalid QoS:
  - `NACK(400, "invalid QoS value")`.
- On success:
  - Creates a subscription and returns `ACK(subscription_id = new_id)`.

### ACK

Used in two main contexts:

- Handshake ACKs:
  - Responses to HELLO and AUTH.
  - `subscription_id` set to `0`.
- QoS1 message ACKs:
  - Client confirms delivery of a previously received PUBLISH.

Payload:

```text
[u64 subscription_id]
```

ACK for QoS1 delivery:

- `subscription_id`:
  - The id returned by SUBSCRIBE.
- `correlation_id`:
  - Must equal the `correlation_id` of the delivered PUBLISH frame (the `DeliveryTag`).

Server behavior on incoming ACK:

- Invalid payload:
  - `NACK(400, "invalid ACK payload")`.
- `subscription_id == 0` (for non-handshake context):
  - `NACK(400, "subscription_id must be non-zero")`.
- If subscription or tag unknown:
  - `NACK(404, "unknown subscription or delivery tag")`.
- On success:
  - Removes message from inflight tracking.
  - Returns no frame.

### NACK

Server → Client error responses.

Payload:

```text
[u16 code][u16 message_len][message_bytes...]
```

- `code`:
  - Numeric error code (see Error Codes).
- `message_len`:
  - Length of message string.
- `message_bytes`:
  - UTF-8 error message.

### POLL

Client → Server.

Payload:

```text
[u64 subscription_id]
```

Server behavior:

- Invalid payload:
  - `NACK(400, "invalid POLL payload")`.
- `subscription_id == 0`:
  - `NACK(400, "subscription_id must be non-zero")`.
- On success:
  - If no message available:
    - Returns no frame; client should backoff and retry.
  - If message available:
    - Returns `PUBLISH` frame carrying the message.
    - For QoS1, `correlation_id` is the delivery tag and must be used in ACK.

### PING / PONG

Keepalive frames.

- PING (Client → Server):
  - Payload: empty.
- PONG (Server → Client):
  - Payload: empty.
  - Sent as a direct response to PING with the same `correlation_id`.

## Error Codes

BlipMQ uses numeric codes in NACK payloads:

| Code | Meaning                                  | Typical Message                                 |
|------|------------------------------------------|-------------------------------------------------|
| 400  | Client error / invalid request           | `"invalid ... payload"`, `"empty topic"`, etc.  |
| 401  | Authentication / authorization failure   | `"unauthenticated"`, `"invalid API key"`        |
| 404  | Unknown resource                         | `"unknown subscription or delivery tag"`        |
| 426  | Protocol version mismatch                | `"unsupported protocol version"`                |
| 500  | Internal server error                    | `"durable publish failed: ..."`                 |

Clients SHOULD:

- Log NACKs with `code` and `message`.
- Treat `400` as client bugs (fix payload and retry).
- Treat `401` as auth misconfiguration (update key).
- Treat `404` as out-of-sync state (e.g. expired subscription or duplicate ACK).
- Treat `426` as protocol version mismatch (update client).
- Treat `500` as transient server issues (retry with backoff).

## Backpressure & Batching Behavior

### Reads

- Server uses `TcpStream::read_buf(&mut read_buf)` to fill a reusable `BytesMut`.
- After each read, `try_decode_frame` is called in a loop:

  - If a frame is fully available, it is dispatched to `MessageHandler`.
  - If partial frame, function returns `Ok(None)` and the loop breaks.

- This supports multiple pipelined frames per TCP read and avoids per-frame allocations.

### Writes

- Response frames are encoded into a reusable `write_buf` (`BytesMut`).
- `flush_write_buffer`:

  - Calls `write` on the TCP stream until `write_buf` is empty.
  - Handles partial writes by advancing the buffer.
  - Returns an error if a `WriteZero` occurs.

- The implementation respects TCP backpressure and avoids busy loops.

### Client Responsibilities

Clients SHOULD:

- Pipeline frames where appropriate (e.g. multiple PUBLISH or POLL frames).
- Use timeouts and backoff when polling (`POLL`) if no data is returned.
- Watch for NACKs and adjust behavior accordingly.

## Hex-Encoded Sample Frames

These examples illustrate on-the-wire representation. All integers are big-endian.

### HELLO (protocol_version = 1, correlation_id = 1)

Payload:

- `protocol_version = 1` → `00 01`.

Total length:

- payload = 2 bytes.
- total = `1 (type) + 8 (cid) + 2 (payload) = 11` → `00 00 00 0B`.

Hex frame:

```text
00 00 00 0B   // length = 11
01            // frame_type = HELLO
00 00 00 00 00 00 00 01   // correlation_id = 1
00 01         // protocol_version = 1
```

### AUTH (api_key = "dev-key", correlation_id = 2)

Key:

- `"dev-key"` bytes = `64 65 76 2D 6B 65 79`.
- `key_len = 7` → `00 07`.

Payload length:

- `2 (len) + 7 (key) = 9`.
- total = `1 + 8 + 9 = 18` → `00 00 00 12`.

Hex:

```text
00 00 00 12   // length = 18
02            // frame_type = AUTH
00 00 00 00 00 00 00 02   // correlation_id = 2
00 07         // key_len = 7
64 65 76 2D 6B 65 79   // "dev-key"
```

### SUBSCRIBE (topic = "demo", qos = 1, correlation_id = 3)

Topic:

- `"demo"` bytes = `64 65 6D 6F`, `topic_len = 4` → `00 04`.

Payload length:

- `2 (len) + 4 (topic) + 1 (qos) = 7`.
- total = `1 + 8 + 7 = 16` → `00 00 00 10`.

Hex:

```text
00 00 00 10   // length = 16
04            // frame_type = SUBSCRIBE
00 00 00 00 00 00 00 03   // correlation_id = 3
00 04         // topic_len = 4
64 65 6D 6F   // "demo"
01            // qos = 1
```

### PUBLISH (Client → Server)

Example: topic = "demo", qos = 1, message = "hi", correlation_id = 4.

Message:

- `"hi"` bytes = `68 69`.

Payload length:

- `1 (qos) + 2 (topic_len) + 4 (topic) + 2 (msg) = 9`.
- total = `1 + 8 + 9 = 18` → `00 00 00 12`.

Hex:

```text
00 00 00 12   // length = 18
03            // frame_type = PUBLISH
00 00 00 00 00 00 00 04   // correlation_id = 4
01            // qos = 1
00 04         // topic_len = 4
64 65 6D 6F   // "demo"
68 69         // "hi"
```

### ACK (Subscription)

Example: ACK for `subscription_id = 42` in response to SUBSCRIBE with `correlation_id = 3`.

Payload:

- `subscription_id = 42` → `00 00 00 00 00 00 00 2A`.

Payload length:

- 8 bytes.
- total = `1 + 8 + 8 = 17` → `00 00 00 11`.

Hex:

```text
00 00 00 11   // length = 17
05            // frame_type = ACK
00 00 00 00 00 00 00 03   // correlation_id = 3
00 00 00 00 00 00 00 2A   // subscription_id = 42
```

These frame examples can be constructed safely using the `blipmq_net` Rust API, which handles encoding/decoding and length calculations.
