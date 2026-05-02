# BlipMQ Binary Protocol

This document describes the BlipMQ TCP wire protocol implemented by `net`.

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
  - Must be = `1 + 8` and = `16 * 1024 * 1024` (16 MiB).
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
| PUBLISH   | 0x03  | Client → Server | Publish a message                         |
| SUBSCRIBE | 0x04  | Client → Server | Subscribe to a topic                      |
| ACK       | 0x05  | Both            | ACK for handshake or QoS1 deliveries      |
| NACK      | 0x06  | Server → Client | Error response                            |
| PING      | 0x07  | Client → Server | Liveness check                            |
| PONG      | 0x08  | Server → Client | Liveness response                         |
| POLL      | 0x09  | Client → Server | (v1 only) Request next message for a sub  |
| DELIVER   | 0x0A  | Server → Client | Server-pushed message delivery (v2)       |

**v1 vs v2:** The `PROTOCOL_VERSION` value sent in the HELLO frame
selects delivery mode for that connection. v1 clients pull messages
via repeated POLL frames; v2 clients receive `DELIVER` frames asynchronously
as the broker fans messages out. The current value of `PROTOCOL_VERSION`
is **2**; v1 is preserved for back-compat with older clients but is no
longer the recommended path.

Unknown frame types result in a decode error and connection close.

## HELLO / AUTH Handshake

### HELLO

Client ? Server.

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

Client ? Server.

Payload:

```text
[u16 key_len][key_bytes...]
```

- `key_len`:
  - Length of the UTF-8 API key in bytes (`= u16::MAX`).
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

Client → Server only. (Server-side delivery to subscribers uses
`DELIVER`, see below.)

Payload:

```text
[u8 qos_flags]
  [u32 ttl_ms          if has_ttl]
  [u16 key_len + key   if has_partition_key]
[u16 topic_len][topic_bytes...]
[message_bytes...]
```

The `qos_flags` byte packs the QoS level and two optional-field flags:

| bit | name              | meaning                                                |
|-----|-------------------|--------------------------------------------------------|
|  0–1| qos               | `0` = AtMostOnce, `1` = AtLeastOnce                    |
|   6 | has_partition_key | when set, a `[u16 key_len][key_bytes]` pair follows    |
|   7 | has_ttl           | when set, a `u32 ttl_ms` field follows the qos byte    |

Order when both flag bits are set: `qos_flags → ttl_ms → key_len + key → topic_len + topic → message`.

- `ttl_ms`: per-message TTL override in milliseconds. Replaces the
  broker's default `message_ttl` for this single message.
- `partition_key`: opaque bytes used as a routing key for sticky-by-key
  routing within consumer groups (same key always lands on the same
  group member). Empty key (`key_len = 0`) is rejected; use
  `has_partition_key = 0` for "no key, round-robin within the group".
- `topic_bytes`: UTF-8 topic name.
- `message_bytes`: opaque payload.

Server behavior on incoming PUBLISH:

- If topic empty: `NACK(400, "empty topic")`.
- If topic is not valid UTF-8 or contains a NUL byte: `NACK(400, ...)`.
- If qos not in {0,1}: `NACK(400, "invalid QoS value")`.
- If WAL channel is full: `NACK(503, "wal_busy")`.
- If QoS1 and WAL configured:
  - Append to WAL, group-commit fsync, then fan out to subscribers.
  - On WAL error: `NACK(500, "durable_publish_failed: ...")`.
- Otherwise:
  - Fan out in-memory only.
- On success:
  - No response frame (fire-and-forget).

### DELIVER

Server → Client. v2-only. Sent asynchronously as the broker fans
messages out to push subscribers; the client does not request it.

Payload:

```text
[u8 qos][u64 delivery_tag][u16 topic_len][topic_bytes...][message_bytes...]
```

- `qos`: 0 or 1 (matches the publishing QoS level).
- `delivery_tag`: per-subscription monotonic tag. For QoS1, the
  client must echo this back as the ACK frame's `correlation_id`.
  Always 0 for QoS0.
- The frame's outer `correlation_id` field also carries `delivery_tag`
  (mirrored), so simple clients can match on either field.

### SUBSCRIBE

Client → Server.

Payload:

```text
[u16 topic_len][topic_bytes...]
[u8 qos_flags]
  [u16 group_len + group  if has_group]
  [u64 from_offset        if has_offset]
```

The `qos_flags` byte packs the QoS level and two optional-field flags:

| bit | name        | meaning                                                |
|-----|-------------|--------------------------------------------------------|
|  0–1| qos         | requested QoS level                                    |
|   6 | has_offset  | when set, a `u64 from_offset` field follows            |
|   7 | has_group   | when set, a `[u16 group_len][group_bytes]` pair follows |

Order when both flag bits are set: `topic → qos_flags → group → from_offset`.

- `group`: name of the consumer group to join. With `has_group = 0`
  this is a broadcast subscriber (every publish to the topic is
  delivered). With `has_group = 1`, the subscriber competes with
  other group members; each publish is delivered to exactly one
  member (round-robin or sticky-by-key).
- `from_offset`: when `has_offset = 1`, the broker walks the WAL
  from this id and re-pushes matching records to the subscriber
  before live publishes start flowing. Special value `0` means
  "earliest". Wildcard subscriptions are not supported on the
  replay path.

Server behavior:

- Invalid payload: `NACK(400, "invalid SUBSCRIBE payload")`.
- Empty topic: `NACK(400, "empty topic")`.
- Invalid QoS: `NACK(400, "invalid QoS value")`.
- Empty group name when `has_group = 1`: `NACK(400, "empty consumer group name")`.
- On success:
  - Registers the subscription on the v2 push path.
  - If `has_offset = 1` and the broker has a WAL: replay records
    from `from_offset` to the subscriber after live registration.
  - Returns `ACK(subscription_id = new_id)`.

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

Server ? Client error responses.

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

### POLL (v1 only — deprecated)

Client → Server. Frame type retained for backward compatibility, but
**v2 SUBSCRIBE registers a push-mode subscription that does not
service POLL**: a v2 client that sends POLL after SUBSCRIBE will get
no message (the broker's poll path returns `None` for slot-mode
subs). v2 clients should read `DELIVER` frames asynchronously
instead.

Payload:

```text
[u64 subscription_id]
```

Server behavior (v1 path):

- Invalid payload: `NACK(400, "invalid POLL payload")`.
- `subscription_id == 0`: `NACK(400, "subscription_id must be non-zero")`.
- On success: returns a `PUBLISH` frame with the next queued message,
  or no frame if the queue is empty.

### PING / PONG

Keepalive frames.

- PING (Client ? Server):
  - Payload: empty.
- PONG (Server ? Client):
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
| 500  | Internal server error                    | `"durable_publish_failed: ..."`                 |
| 503  | Backpressure / transient overload        | `"wal_busy"`, `"wal_writer_stopped"`            |

Clients SHOULD treat 503 as transient: retry with exponential
backoff, don't crash.

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

- `protocol_version = 1` ? `00 01`.

Total length:

- payload = 2 bytes.
- total = `1 (type) + 8 (cid) + 2 (payload) = 11` ? `00 00 00 0B`.

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
- `key_len = 7` ? `00 07`.

Payload length:

- `2 (len) + 7 (key) = 9`.
- total = `1 + 8 + 9 = 18` ? `00 00 00 12`.

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

- `"demo"` bytes = `64 65 6D 6F`, `topic_len = 4` ? `00 04`.

Payload length:

- `2 (len) + 4 (topic) + 1 (qos) = 7`.
- total = `1 + 8 + 7 = 16` ? `00 00 00 10`.

Hex:

```text
00 00 00 10   // length = 16
04            // frame_type = SUBSCRIBE
00 00 00 00 00 00 00 03   // correlation_id = 3
00 04         // topic_len = 4
64 65 6D 6F   // "demo"
01            // qos = 1
```

### PUBLISH (Client ? Server)

Example: topic = "demo", qos = 1, message = "hi", correlation_id = 4.

Message:

- `"hi"` bytes = `68 69`.

Payload length:

- `1 (qos) + 2 (topic_len) + 4 (topic) + 2 (msg) = 9`.
- total = `1 + 8 + 9 = 18` ? `00 00 00 12`.

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

- `subscription_id = 42` ? `00 00 00 00 00 00 00 2A`.

Payload length:

- 8 bytes.
- total = `1 + 8 + 8 = 17` ? `00 00 00 11`.

Hex:

```text
00 00 00 11   // length = 17
05            // frame_type = ACK
00 00 00 00 00 00 00 03   // correlation_id = 3
00 00 00 00 00 00 00 2A   // subscription_id = 42
```

These frame examples can be constructed safely using the `net` Rust API, which handles encoding/decoding and length calculations.

