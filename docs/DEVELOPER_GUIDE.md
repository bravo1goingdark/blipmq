# BlipMQ Developer Guide

This guide explains the BlipMQ workspace layout, concurrency model, coding patterns, and how to extend the protocol and broker safely.

## Workspace Layout

From the workspace root `Cargo.toml`, the main crates are:

- `blipmqd`
  - Main broker daemon.
  - Wires together config, WAL, broker, net server, metrics, and shutdown.
- `core`
  - Core broker logic:
    - Topics and subscriptions.
    - Per-subscriber queues.
    - QoS0/QoS1.
    - TTL and retry with exponential backoff.
    - WAL integration for durability.
- `net`
  - TCP networking and binary framing:
    - Frame types: HELLO, AUTH, PUBLISH, SUBSCRIBE, ACK, NACK, PING, PONG, POLL.
    - Per-connection `Connection` tasks.
    - `Server` accept loop.
    - `MessageHandler` trait and `BrokerHandler` implementation.
- `wal`
  - Write-ahead log:
    - Append-only, CRC32-checked, with index rebuild on open.
    - Configurable fsync policy.
- `auth`
  - Static API key validation.
- `metrics`
  - HTTP `/metrics` endpoint.
- `config`
  - Config loader (file + env merge).
- `bench`
  - In-process performance harness using the real protocol over TCP.
- `chaos`
  - Chaos and failure testing utilities.

Integration tests live under the root `tests/` directory.

## Concurrency Model

BlipMQ uses Tokio for async I/O and `parking_lot` locks for in-memory data structures, combined with `Arc` for shared ownership.

### Networking

- The accept loop is a single async task in `net::Server`:
  - Binds a `TcpListener` to `Config.bind_addr:port`.
  - On each `accept()`:
    - Creates a `Connection` with:
      - The `TcpStream`.
      - A cloned `MessageHandler` implementation (typically `BrokerHandler`).
      - A cloned `ApiKeyValidator`.
      - A cloned shutdown `watch` receiver.
    - Spawns `connection.run()` as a Tokio task.
- `Connection`:
  - Maintains `read_buf: BytesMut` and `write_buf: BytesMut` for reuse.
  - Uses `read_buf` with `read_buf(&mut read_buf)` and decodes frames via `try_decode_frame`.
  - Handles `HELLO` and `AUTH` locally, using the `ApiKeyValidator`.
  - For application frames, calls `handler.handle_frame(conn_id, frame).await`.

### Broker

- The broker core is encapsulated in `corelib::Broker`:
  - Shared as `Arc<Broker>` between:
    - Network handler (`BrokerHandler`).
    - Metrics server.
    - Maintenance scheduler task.
- Topic registry via `TopicShards`:
  - A vector of `RwLock<HashMap<TopicName, Arc<Topic>>>`.
  - Topic shard index is `hash(topic) % num_shards`.
  - This reduces lock contention on multi-topic workloads.
- Per-subscriber queues:
  - Each `Subscriber` has a `SubscriberQueue` that wraps:
    - `Mutex<SubscriberQueueInner>`.
    - `SubscriberQueueInner` contains:
      - `pending: VecDeque<QueueEntry>`.
      - `inflight: HashMap<DeliveryTag, QueueEntry>`.
  - `enqueue`, `dequeue`, `ack`, and `maintenance_tick` are kept small, use `Bytes` for payloads, and avoid unnecessary copying.

### WAL

- The WAL is provided by `wal::WriteAheadLog`:
  - Wraps a `WalInner` inside `tokio::sync::Mutex`.
  - Ensures:
    - Sequential appends.
    - Consistent index updates.
    - Controlled fsync behavior via `WalConfig`.
- Typical append path:
  - Compute record header (`id`, `len`, `crc32`).
  - Write header + payload to file.
  - Update in-memory index and metrics.
  - Call `maybe_sync` to decide whether to flush and `sync_data()`.

### Scheduler

`blipmqd` runs a maintenance task that periodically drives TTL and retry logic:

```rust
let maintenance_broker = broker.clone();
let mut maintenance_shutdown = shutdown_tx.subscribe();
tokio::spawn(async move {
    let mut interval = time::interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                maintenance_broker.maintenance_tick(Instant::now());
            }
            changed = maintenance_shutdown.changed() => {
                if changed.is_ok() {
                    break;
                } else {
                    break;
                }
            }
        }
    }
});
```

This keeps scheduled work out of the core publish and poll paths.

## Coding Patterns and Guidelines

Key principles:

- Prefer small, focused modules and types over large, monolithic designs.
- Use `Bytes` and `BytesMut` for message payloads and framing buffers to minimize allocations and copies.
- Use `Arc` for shared state and `parking_lot` locks to keep critical sections short.
- Use `thiserror` for structured error handling.
- Avoid panics in normal code paths; tests may use `panic!` when asserting behavior.
- Use `tracing` spans for hot paths:
  - `#[tracing::instrument(skip(...))]` on key async functions.
  - `trace_span!` for inner-block profiling (e.g. dequeue, WAL flush).

Testing practice:

- Unit tests within each crate:
  - `core`:
    - QoS0 and QoS1 publish/subscribe.
    - TTL expiry and retry logic.
    - WAL-based durability.
  - `wal`:
    - Append/read roundtrip.
    - Corruption detection.
    - Index rebuild on open.
  - `net`:
    - Frame encode/decode roundtrips.
    - Partial buffer behavior.
- Integration tests under `tests/`:
  - Daemon-level tests for HELLO/AUTH, SUBSCRIBE, PUBLISH, POLL, ACK.
  - Multiple subscribers and error case handling.
  - Chaos and crash-recovery tests using `chaos`.

## Extending the Protocol

This section walks through adding a new frame type end-to-end, using a hypothetical `STATS` frame that allows clients to query broker statistics.

### 1. Extend `FrameType`

In `net/src/frame.rs`, add a variant:

```rust
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
    Stats = 0x0A,
}
```

Update the `TryFrom<u8>` implementation to map `0x0A` to `FrameType::Stats`.

### 2. Add Payload Types

Still in `frame.rs`, define request and response payloads:

```rust
/// STATS request payload: currently empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsRequestPayload;

impl StatsRequestPayload {
    pub fn encode(&self) -> Bytes {
        Bytes::new()
    }

    pub fn decode(payload: &Bytes) -> Result<Self, FrameDecodeError> {
        if !payload.is_empty() {
            return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
        }
        Ok(Self)
    }
}

/// STATS response payload:
/// [u64 topics][u64 subscribers][u64 messages_published][u64 messages_delivered][u64 inflight]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsResponsePayload {
    pub topics: u64,
    pub subscribers: u64,
    pub messages_published: u64,
    pub messages_delivered: u64,
    pub inflight: u64,
}

impl StatsResponsePayload {
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(8 * 5);
        buf.put_u64(self.topics);
        buf.put_u64(self.subscribers);
        buf.put_u64(self.messages_published);
        buf.put_u64(self.messages_delivered);
        buf.put_u64(self.inflight);
        buf.freeze()
    }

    pub fn decode(payload: &Bytes) -> Result<Self, FrameDecodeError> {
        if payload.len() != 8 * 5 {
            return Err(FrameDecodeError::InvalidLength(payload.len() as u32));
        }
        let mut slice = &payload[..];
        Ok(Self {
            topics: slice.get_u64(),
            subscribers: slice.get_u64(),
            messages_published: slice.get_u64(),
            messages_delivered: slice.get_u64(),
            inflight: slice.get_u64(),
        })
    }
}
```

Re-export these from `net/src/lib.rs` so clients can use them.

### 3. Handle the New Frame in `BrokerHandler`

In `net/src/server.rs`, extend the `MessageHandler` implementation:

```rust
#[async_trait]
impl MessageHandler for BrokerHandler {
    async fn handle_frame(
        &self,
        conn_id: u64,
        frame: Frame,
    ) -> Result<FrameResponse, Error> {
        match frame.msg_type {
            FrameType::Publish => self.handle_publish(conn_id, frame).await,
            FrameType::Subscribe => self.handle_subscribe(conn_id, frame).await,
            FrameType::Ack => self.handle_ack(conn_id, frame).await,
            FrameType::Ping => {
                let pong = Frame {
                    msg_type: FrameType::Pong,
                    correlation_id: frame.correlation_id,
                    payload: Bytes::new(),
                };
                Ok(FrameResponse::Frame(pong))
            }
            FrameType::Poll => self.handle_poll(conn_id, frame).await,
            FrameType::Stats => self.handle_stats(conn_id, frame).await,
            FrameType::Pong | FrameType::Nack | FrameType::Hello | FrameType::Auth => {
                Ok(FrameResponse::None)
            }
        }
    }
}
```

Add `handle_stats`:

```rust
impl BrokerHandler {
    async fn handle_stats(
        &self,
        _conn_id: u64,
        frame: Frame,
    ) -> Result<FrameResponse, Error> {
        use crate::frame::{StatsRequestPayload, StatsResponsePayload};

        if StatsRequestPayload::decode(&frame.payload).is_err() {
            let nack = self.make_nack(
                frame.correlation_id,
                400,
                "invalid STATS payload",
            )?;
            return Ok(FrameResponse::Frame(nack));
        }

        let topics = self.broker.topic_count() as u64;
        let subscribers = self.broker.subscriber_count() as u64;
        let published = self.broker.messages_published_total();
        let delivered = self.broker.messages_delivered_total();
        let inflight = self.broker.inflight_message_count();

        let payload = StatsResponsePayload {
            topics,
            subscribers,
            messages_published: published,
            messages_delivered: delivered,
            inflight,
        }
        .encode();

        let response = Frame {
            msg_type: FrameType::Stats,
            correlation_id: frame.correlation_id,
            payload,
        };

        Ok(FrameResponse::Frame(response))
    }
}
```

### 4. Document the New Frame

Update `docs/PROTOCOL.md` to:

- Add a `STATS` entry to the frame type table.
- Describe request and response payloads.
- Clarify that the response fields correspond to what `/metrics` exposes (topics, subscribers, published, delivered, inflight).

### 5. Add Tests

- Unit tests in `net`:
  - Roundtrip encode/decode of `StatsRequestPayload` and `StatsResponsePayload`.
- Integration test in `tests/`:
  - Start a test server.
  - Connect with a client (HELLO/AUTH).
  - Send a STATS request.
  - Decode response and validate fields using:
    - Direct broker calls (`topic_count`, `subscriber_count`, etc.), or
    - Comparison with the metrics endpoint.

## Extending the Broker

When modifying or extending `core`:

- Keep hot paths (`publish`, `publish_durable`, `poll`, `ack`, queue operations) as allocation-free as feasible.
- Use `Bytes` and `BytesMut` for payloads and record encoding, not `Vec<u8>` where avoidable.
- Avoid holding locks longer than necessary:
  - Do not perform I/O (e.g. WAL operations) while holding `Mutex`/`RwLock`.
  - Encode records and compute metadata outside lock scopes when possible.
- When adding metrics:
  - Use `AtomicU64` counters on `Broker` or `WriteAheadLog`.
  - Expose them via accessor methods and integrate them into `metrics`.

If you introduce new durable state:

- Consider whether it needs to be encoded into WAL:
  - If yes, extend `WalMessageRecord` or create new record types with explicit versioning.
- Update recovery logic in `Broker::replay_from_wal`.
- Extend tests:
  - Broker-level durable tests in `core`.
  - Crash-recovery tests in `tests/chaos_recovery.rs`.

## Running and Debugging Tests

From the workspace root:

```bash
cargo test
```

This runs:

- Core broker tests (`core`).
- WAL tests (`wal`).
- Networking/framing tests (`net`).
- Integration tests (`tests/daemon_integration.rs`, `tests/chaos_recovery.rs`).

Tips:

- Use `RUST_LOG=debug` or `RUST_LOG=trace` for specific modules when debugging:

  ```bash
  RUST_LOG=core=debug,net=debug cargo test tests::daemon_integration -- --nocapture
  ```

- Use `tokio-console` in development by setting `enable_tokio_console = true` in config to inspect async task behavior.


