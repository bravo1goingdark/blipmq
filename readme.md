<p align="center">
  <img src="./assets/readme-banner.png" alt="BlipMQ Logo" width="1200" />
</p>

<p align="center">
  <b>BlipMQ</b> — a fast, durable, single-binary message broker in Rust.
  Push pub/sub with QoS, write-ahead log, sub-millisecond fanout.
</p>

<p align="center">
  <a href="https://blipmq.dev"><strong>Website</strong></a> ·
  <a href="https://github.com/bravo1goingdark/blipmq"><strong>GitHub</strong></a> ·
  <a href="https://x.com/blipmq"><strong>Twitter</strong></a> ·
  <a href="https://linkedin.com/company/blipmq"><strong>LinkedIn</strong></a>
</p>

<p align="center">
  <a href="#performance"><img src="https://img.shields.io/badge/multi--pipe-6.3M%20msg%2Fs-brightgreen" alt="multi-pipe throughput"></a>
  <a href="#performance"><img src="https://img.shields.io/badge/fanout-5.4M%20deliv%2Fs-brightgreen" alt="fanout"></a>
  <a href="#durability"><img src="https://img.shields.io/badge/durability-fsync%E2%80%93gated-blue" alt="durability"></a>
  <img src="https://img.shields.io/badge/license-MIT%20%7C%20Apache--2.0-blue" alt="license">
  <img src="https://img.shields.io/badge/edition-2021-orange" alt="rust">
</p>

---

## Why BlipMQ

- **Push-based delivery, not poll.** v2 protocol delivers messages to subscribers as `DELIVER` frames the moment a publish lands; subscribers don't long-poll. A single TCP pipe sustains ~1.8 M msg/s on loopback; 16 parallel pipes aggregate to **~6.3 M msg/s** on a dev box.
- **Real durability for QoS1.** `publish_durable` returns *only after* the record is on disk (group-commit fsync). An in-band ack journal + checkpoint snapshots let a restarted broker re-deliver only the unacked tail; a background compactor reclaims fully-acked WAL segments.
- **NATS-style subject wildcards.** Subscribers can use `*` (one token) and `>` (rest) in their pattern; exact-match subscriptions stay on the O(1) lookup path.
- **One static binary.** `blipmqd` ships as a single Rust binary; no JVM, no ZooKeeper, no external dependencies. Optional `tls` feature enables TLS termination via `tokio-rustls`.
- **Lock-light hot path.** Sharded subscription registry, per-thread sharded counters with cache-line padding, snapshot-based fanout, AHash routing, mimalloc allocator. The publish path takes no broker-wide mutex and only one brief per-subscriber lock to encode the DELIVER frame.

## Status

Pre-1.0. The wire protocol is at version `2` and is reasonably stable. Recent hardening: TLS, slow-consumer policies, NACK 503 backpressure, per-message TTL, dead-letter routing, auth rate limiting. See [PROD_READINESS.md](./PROD_READINESS.md) for the current readiness checklist.

## Performance

All numbers are real-TCP-loopback benches on a single dev machine with `cargo bench`. Reproduce locally:

```bash
cargo bench -p net --bench wire_throughput   # M1: 1 pub → 1 sub, single pipe
cargo bench -p net --bench wire_fanout       # M2: 1 pub × N subs
cargo bench -p net --bench wire_multi_pub    # N parallel (pub, sub) pipes
cargo bench -p core --bench fanout           # in-process broker fanout
```

| benchmark | shape | result |
|---|---|---:|
| `wire_throughput`, 1M × 64B | 1 pub → 1 sub, QoS0 | **~1.8 M msg/s** sustained |
| `wire_throughput`, 200K × 1 KiB | 1 pub → 1 sub, QoS0 | **~880 MiB/s** |
| `wire_fanout`, 128 × 25K × 64B | 1 pub × 128 subs, QoS0 | **~5.3 M deliv/s** total |
| `wire_multi_pub`, 8 pipes × 62.5K × 64B | 8 parallel `(pub, sub)` pairs | **~5.7–6.3 M msg/s** aggregate |
| `wire_multi_pub`, 16 pipes × 31.25K × 64B | 16 parallel pairs | **~6.3 M msg/s** aggregate |
| QoS1 durable publish, fsync-gated | `publish_durable` → on-disk ACK | covered by group-commit batching |

The multi-pipe ladder shows where lock-free sharding pays off: at 4+ publishers the per-publisher counters and per-topic counters were the bottleneck, and after sharding (commits `7538947`, `bc3e432`) aggregate scales nearly linearly with cores up to ~16 pipes.

Detailed methodology, per-frame budget breakdown, and the full lever ladder for further perf work are in [`bench_compare/results/`](./bench_compare/results/).

## Quick start

### Run from source

```bash
cargo run -p blipmqd -- --config ./config/blipmq-dev.toml
```

`CONFIG=./config/blipmq-dev.toml cargo run -p blipmqd` works too.

### Connect with the wire protocol

After `HELLO` (with `protocol_version = 2`) and `AUTH`, you can `SUBSCRIBE` to a topic and `PUBLISH` messages. Subscribers receive `DELIVER` frames asynchronously (no poll loop). See [`docs/PROTOCOL.md`](./docs/PROTOCOL.md) for the full frame spec.

## Architecture

```
                ┌────────────────────────────────────┐
                │              blipmqd               │
                │                                    │
   TCP/v2 ─►   │  net  ─►  core (broker)  ─►  wal   │  ─► segmented .log dir
                │  └─ reader/writer task pair       │
                │     per connection                 │
   TCP/v2 ◄─   │  ◄─ shared push slot               │
                │     (BytesMut + Notify)            │
                │                                    │
                │  metrics  /healthz  /readyz        │  ─► Prometheus
                │           /metrics                 │
                └────────────────────────────────────┘
```

### Workspace layout

| crate | role |
|---|---|
| `blipmqd` | broker daemon binary (TCP, WAL, auth, metrics, graceful shutdown, mimalloc) |
| `core` | broker logic — topics, sharded subscriptions, push slot, QoS, retry, TTL, DLQ |
| `net` | binary frame protocol + TCP server with reader/writer split, optional TLS |
| `wal` | segmented write-ahead log; CRC-checked, group-commit fsync, ack journal |
| `auth` | static API key validator (pluggable trait) |
| `metrics` | HTTP endpoint exposing Prometheus-format counters + healthz/readyz |
| `config` | TOML/YAML loader with env overrides; partial-config rejection |
| `chaos` | fault-injection helpers (corruption, slow disk, disconnect) |
| `bench` | in-process broker microbench (publishers/subscribers/topics) |
| `bench_compare` | comparison harness against NATS over TCP, plus result archives |

### Hot path (push delivery)

1. Publisher `write_all`s a batched stream of `PUBLISH` frames.
2. The connection's reader task decodes, calls a sync fast path that bypasses `async_trait` dispatch, and resolves the topic via a per-conn `Arc<str>` cache.
3. The broker locates the topic in a sharded map (AHash), snapshots its subscribers under a brief read lock, and for each push subscriber:
   - increments a per-subscriber atomic delivery tag,
   - locks the per-conn shared `BytesMut` for ~tens of nanoseconds,
   - encodes the `DELIVER` frame inline,
   - calls `Notify::notify_one()`.
4. The connection's writer task wakes, swaps the shared buffer for an empty local one, and `write_all`s the local buffer in a single syscall.

No per-frame channel hop, no per-frame heap allocation in the writer, no `async_trait` `Box<dyn Future>` on the PUBLISH path. Topic names are interned as `Arc<str>` so fanout-side cloning is a refcount bump. Broker-wide and per-topic counters (`messages_published_total`, `delivered_total`, `publish_count`) are sharded by publisher-thread index over cache-line-padded `AtomicU64`s, so concurrent publishers don't ping-pong a shared counter line.

See [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md) for the full design.

## Configuration

Minimal TOML (`./config/blipmq-dev.toml`):

```toml
bind_addr     = "127.0.0.1"
port          = 7878

metrics_addr  = "127.0.0.1"
metrics_port  = 9090

# WAL is a directory of segments (wal-NNNNNNNNNNNNNNNNNNNN.log).
# Old single-file WALs (pre-segmentation) must be drained before upgrading.
wal_path      = "./blipmq-wal"
fsync_policy  = "every_n:1"        # or "always", "none", "interval_ms:50"

max_retries       = 3
retry_backoff_ms  = 100

allowed_api_keys  = ["dev-key-1", "dev-key-2"]

# Optional TLS (requires `--features tls` on the net crate). Both must be
# set together; setting only one is rejected at startup.
# tls_cert_path = "./certs/server.crt"
# tls_key_path  = "./certs/server.key"
```

Env overrides (each maps to the field above, screaming-snake-case):

```
BIND_ADDR, PORT
METRICS_ADDR, METRICS_PORT
WAL_PATH, FSYNC_POLICY
MAX_RETRIES, RETRY_BACKOFF_MS
ALLOWED_API_KEYS="key1,key2,..."
TLS_CERT_PATH, TLS_KEY_PATH
```

Full reference: [`docs/CONFIG.md`](./docs/CONFIG.md).

## Durability

`wal/` implements a segmented append-only log:

- **Directory of segments**: `wal-00000000000000000001.log`, `…00000002.log`, … rolling at `WalConfig::segment_bytes` (default 256 MiB). Each segment carries its own header.
- **Record layout**: `[id u64][len u32][crc32 u32][payload]`. CRC32 covers `(id, len, payload)` so a flip in the framing bytes is caught on replay.
- **Group-commit fsync**: `WriteAheadLog::append_durable(Bytes)` returns only after `fsync` covers the record. The broker's `publish_durable` uses this; QoS0 publishes use the channel-gated `append` for speed.
- **Ack journal**: every QoS1 ACK writes a typed `Ack` record to the WAL `(client_id, topic, acked_wal_id)`. On restart, replay walks the WAL in two passes: first builds per-`(client_id, topic)` "highest acked wal_id" cursors, then re-enqueues only Message records past each consumer's cursor. A client that ACKed 80% before a crash sees only the unacked 20% on restart.
- **Checkpoint snapshots + compaction**: `blipmqd` periodically writes an atomic checkpoint of all consumer cursors and asks the WAL to delete segments fully covered by the safe threshold. Replay reads the latest checkpoint first, then only the tail.
- **Corruption detection**: header magic + version + per-record CRC. Torn / partial trailing records are treated as not-present rather than fatal.

See [`docs/RECOVERY.md`](./docs/RECOVERY.md).

## Observability

`metrics` exposes a Prometheus-text-format endpoint plus liveness/readiness probes:

- `GET /metrics` — Prometheus exposition (text version 0.0.4) with `# HELP` / `# TYPE` headers. Selected series:

```text
blipmq_topics_total
blipmq_subscribers_total
blipmq_messages_published_total
blipmq_messages_delivered_total
blipmq_messages_inflight
blipmq_push_dropped_total            # slow-consumer drops on the push path
blipmq_topic_published_total{topic="…"}
blipmq_topic_delivered_total{topic="…"}
blipmq_publish_fanout_seconds        # hdrhistogram summary, 1-in-64 sampled
blipmq_wal_appends_total
blipmq_wal_bytes_total
```

- `GET /healthz` — liveness (200 if the process is up).
- `GET /readyz` — readiness (200 once WAL replay completes and the broker is accepting publishes).

With `enable_tokio_console = true` in config, you can attach `tokio-console` for per-task visibility.

[`docs/OPERATIONS.md`](./docs/OPERATIONS.md) covers metrics, alerts, and runbooks.

## QoS, TTL, retry, and DLQ

`core` supports:

- **QoS 0** (at-most-once) and **QoS 1** (at-least-once),
- **per-message TTL**: PUBLISH carries an optional `ttl_ms` (bit 7 of the qos byte signals presence) that overrides the broker default for that single message,
- **dead-letter topic**: messages that exceed `max_retries` or hit their TTL while inflight are re-published at QoS0 to `<original_topic><dlq_suffix>` (default `.dlq`) so subscribers can react,
- per-message metadata: `created_at`, optional `ttl`, `delivery_attempts`, `next_delivery_at`,
- periodic maintenance via `Broker::maintenance_tick`:
  - drops expired messages (TTL),
  - reschedules unacked QoS1 messages with exponential backoff,
  - DLQs after `max_retries`.

`blipmqd` runs the maintenance loop on a Tokio interval and respects graceful shutdown.

## Slow-consumer policy and backpressure

- **Per-subscriber buffer cap.** When a push subscriber's shared `BytesMut` grows past `slow_consumer_buffer_bytes`, the broker stops appending DELIVER frames for that subscriber and either drops the new message (`DropNewest`, default) or unsubscribes the connection (`DropSubscription`). Drops increment `blipmq_push_dropped_total`.
- **WAL backpressure → NACK 503.** When the WAL writer's channel is full, durable publishes return `NACK code=503 "wal_busy"` to the publisher rather than blocking the broker. Publishers can retry with backoff.
- **Auth rate limiting.** Repeated AUTH failures on a connection trigger exponential backoff and an eventual close, rather than allowing unbounded brute-force.

## Graceful shutdown

On `SIGINT`/`SIGTERM` (Ctrl+C), `blipmqd`:

1. marks the broker as shutting down (no new publishes accepted),
2. signals the network server, maintenance, and checkpoint tasks to stop,
3. waits up to a bounded timeout for in-flight QoS1 messages to drain (notify-driven, not poll-based),
4. flushes the WAL, then exits.

This preserves at-least-once semantics across restarts without abrupt loss of in-flight messages.

## Development

### Tests

```bash
cargo test --workspace          # full suite (40 tests, ~1s)
cargo test -p core              # broker logic
cargo test -p net               # frame + connection + push delivery (+ TLS w/ --features tls)
cargo test -p wal               # segmented WAL, CRC, durability, rollover
```

### Benches

```bash
# Wire-level (real TCP loopback):
cargo bench -p net --bench wire_throughput
cargo bench -p net --bench wire_fanout
cargo bench -p net --bench wire_multi_pub

# In-process broker fanout (Criterion):
cargo bench -p core --bench fanout

# In-process publisher/subscriber harness with optional WAL:
cargo run --release -p bench -- \
  --publishers 4 --subscribers 4 --topics 8 \
  --messages 50000 --msg-size 256 --qos 1 --wal
```

### Profiling

```bash
cargo install flamegraph
cargo build -p blipmqd --release

# Record perf data (may require sudo on Linux).
sudo perf record -F 99 -g -- target/release/blipmqd --config ./config/blipmq-dev.toml

# Generate flamegraph from the perf data.
sudo flamegraph --perfdata perf.data
```

Hot paths (`publish_with_wal_id`, `encode_frame`, `try_decode_frame`, WAL append) are annotated with `#[inline(always)]`. We deliberately do **not** wrap them in `#[tracing::instrument]` — even when filtered out, the macro creates and enters a span per call, which is measurable at multi-M ops/sec. If you need trace-level visibility on the hot path, add it back behind a `cfg(debug_assertions)` gate. [`docs/DEVELOPER_GUIDE.md`](./docs/DEVELOPER_GUIDE.md) has more.

## Roadmap

The high-level phases are tracked in `bench_compare/results/`:

- **Phase 1 — net hot path** ✅ — push delivery, batched writer, TCP_NODELAY, zero-copy decode
- **Phase 2 — core fanout** ✅ — sharded subscriptions, snapshot fanout, AHash, shared push slot
- **Phase 3 — WAL durability** ✅ — group-commit fsync, segmented log, CRC over header
- **Phase 4 — recovery** ✅ — ack journal, fast replay, checkpoint snapshots, background segment compaction
- **Phase 5 — observability** ✅ — Prometheus exposition, hdrhistogram fanout summary, per-topic counters, healthz/readyz
- **Phase 6 — production hardening** ✅ — NACK 503 on backpressure, slow-consumer policy, topic validation, TLS via `tokio-rustls`, auth rate limiting, mimalloc allocator
- **Phase 7 — feature unification** (in progress) — NATS-style wildcards `*` / `>` ✅, RabbitMQ-style DLQ + per-message TTL ✅, Kafka-style consumer groups + offset replay pending

Recent perf work (in-progress, on `main`):

- Cache-line padding on hot broker atomics (`4a9cea5`)
- Per-thread sharded broker counters (`7538947`)
- Per-thread sharded per-topic counters (`bc3e432`)
- Stripped `tracing::instrument` from hot paths (`9026b15`)

## License

Dual-licensed under [MIT](./LICENSE-MIT) or [Apache 2.0](./LICENSE-APACHE) at your option.
