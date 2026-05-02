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
  <a href="#performance"><img src="https://img.shields.io/badge/throughput-7.28M%20msg%2Fs-brightgreen" alt="throughput"></a>
  <a href="#performance"><img src="https://img.shields.io/badge/fanout-5.40M%20deliv%2Fs-brightgreen" alt="fanout"></a>
  <a href="#durability"><img src="https://img.shields.io/badge/durability-fsync%E2%80%93gated-blue" alt="durability"></a>
  <img src="https://img.shields.io/badge/license-MIT%20%7C%20Apache--2.0-blue" alt="license">
  <img src="https://img.shields.io/badge/edition-2021-orange" alt="rust">
</p>

---

## Why BlipMQ

- **Push-based delivery, not poll.** v2 protocol delivers messages to subscribers as `DELIVER` frames the moment a publish lands; subscribers don't long-poll. Single TCP pipe sustains ~1.9 M msg/s on loopback; **8 parallel pipes aggregate to 7.28 M msg/s** on a dev box.
- **Real durability for QoS1.** `publish_durable` returns *only after* the record is on disk (group-commit fsync). An in-band ack journal + checkpoint snapshots let a restarted broker re-deliver only the unacked tail; a background compactor reclaims fully-acked WAL segments.
- **NATS-style subject wildcards.** Subscribers can use `*` (one token) and `>` (rest) in their pattern; exact-match subscriptions stay on the O(1) lookup path.
- **One static binary.** `blipmqd` ships as a single Rust binary; no JVM, no ZooKeeper, no external dependencies.
- **Production tooling.** Sharded subscriptions, segmented WAL with CRC32 over headers + payload, snapshot-based fanout, AHash routing, Prometheus-friendly metrics endpoint.

## Status

Pre-1.0. The wire protocol is at version `2` and is reasonably stable; the broker is being hardened against production failure modes (slow consumers, crash recovery, retention/compaction). See [PROD_READINESS.md](./PROD_READINESS.md) for the current readiness checklist.

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
| `wire_throughput`, 1M × 64B | 1 pub → 1 sub, QoS0 | **1.82 M msg/s** sustained |
| `wire_fanout`, 64 × 50K × 64B | 1 pub × 64 subs, QoS0 | **5.40 M deliv/s** total |
| `wire_multi_pub`, 8 pipes × 62.5K × 64B | 8 parallel `(pub, sub)` pairs | **7.28 M msg/s** aggregate |
| `core::fanout`, 64 push subs | in-process publish + fanout | 5.35 M elem/s |
| QoS1 durable publish, fsync-gated | `publish_durable` → on-disk ACK | covered by group-commit batching |

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
                │  metrics  /healthz  /metrics       │  ─► Prometheus
                └────────────────────────────────────┘
```

### Workspace layout

| crate | role |
|---|---|
| `blipmqd` | broker daemon binary (TCP, WAL, auth, metrics, graceful shutdown) |
| `core` | broker logic — topics, sharded subscriptions, push slot, QoS, retry, TTL |
| `net` | binary frame protocol + TCP server with reader/writer split |
| `wal` | segmented write-ahead log; CRC-checked, group-commit fsync, ack journal |
| `auth` | static API key validator (pluggable trait) |
| `metrics` | HTTP endpoint exposing broker + WAL counters |
| `config` | TOML/YAML loader with env overrides |
| `chaos` | fault-injection helpers (corruption, slow disk, disconnect) |
| `bench` | in-process broker microbench (publishers/subscribers/topics) |
| `bench_compare` | comparison harness against NATS over TCP, plus result archives |

### Hot path (push delivery)

1. Publisher `write_all`s a batched stream of `PUBLISH` frames.
2. The connection's reader task decodes, calls a sync fast path that bypasses `async_trait` dispatch, and resolves the topic via a per-conn `Arc<str>` cache.
3. The broker locates the topic in a sharded map (AHash), snapshots its subscribers under a brief read lock, and for each push subscriber:
   - increments an atomic delivery tag,
   - locks the per-conn shared `BytesMut` for ~tens of nanoseconds,
   - encodes the `DELIVER` frame inline,
   - calls `Notify::notify_one()`.
4. The connection's writer task wakes, swaps the shared buffer for an empty local one, and `write_all`s the local buffer in a single syscall.

No per-frame channel hop, no per-frame heap allocation in the writer, no `async_trait` `Box<dyn Future>` on the PUBLISH path. Topic names are interned as `Arc<str>` so fanout-side cloning is a refcount bump.

See [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md) for the full design.

## Configuration

Minimal TOML (`./config/blipmq-dev.toml`):

```toml
bind_addr     = "127.0.0.1"
port          = 7878

metrics_addr  = "127.0.0.1"
metrics_port  = 9090

# WAL is now a directory of segments (wal-NNNNNNNNNNNNNNNNNNNN.log).
# Bumped from v2 single-file format. Old single-file WALs must be
# drained before upgrading.
wal_path      = "./blipmq-wal"
fsync_policy  = "every_n:1"        # or "always", "none", "interval_ms:50"

max_retries       = 3
retry_backoff_ms  = 100

allowed_api_keys  = ["dev-key-1", "dev-key-2"]
```

Env overrides (each maps to the field above, screaming-snake-case):

```
BIND_ADDR, PORT
METRICS_ADDR, METRICS_PORT
WAL_PATH, FSYNC_POLICY
MAX_RETRIES, RETRY_BACKOFF_MS
ALLOWED_API_KEYS="key1,key2,..."
```

Full reference: [`docs/CONFIG.md`](./docs/CONFIG.md).

## Durability

`wal/` implements a segmented append-only log:

- **Directory of segments**: `wal-00000000000000000001.log`, `…00000002.log`, … rolling at `WalConfig::segment_bytes` (default 256 MiB). Each segment carries its own header.
- **Record layout**: `[id u64][len u32][crc32 u32][payload]`. CRC32 covers `(id, len, payload)` so a flip in the framing bytes is caught on replay.
- **Group-commit fsync**: `WriteAheadLog::append_durable(Bytes)` returns only after `fsync` covers the record. The broker's `publish_durable` uses this; QoS0 publishes use the channel-gated `append` for speed.
- **Ack journal**: every QoS1 ACK writes a typed `Ack` record to the WAL `(client_id, topic, acked_wal_id)`. On restart, replay walks the WAL in two passes: first builds per-`(client_id, topic)` "highest acked wal_id" cursors, then re-enqueues only Message records past each consumer's cursor. A client that ACKed 80% before a crash sees only the unacked 20% on restart.
- **Corruption detection**: header magic + version + per-record CRC. Torn / partial trailing records are treated as not-present rather than fatal.

See [`docs/RECOVERY.md`](./docs/RECOVERY.md).

## Observability

`metrics` exposes a small HTTP endpoint suitable for Prometheus scraping:

- `GET /metrics` on `metrics_addr:metrics_port`:

```text
topics                    <n>
subscribers               <n>
messages_published_total  <n>
messages_delivered_total  <n>
messages_inflight         <n>
push_dropped_total        <n>      # slow-consumer drops on the push path
wal_appends_total         <n>
wal_bytes_total           <n>
```

Tracing spans are emitted on the publish, frame encode/decode, and WAL append paths. With `enable_tokio_console = true` in config, you can attach `tokio-console` for per-task visibility.

[`docs/OPERATIONS.md`](./docs/OPERATIONS.md) covers metrics, alerts, and runbooks.

## QoS, TTL, and retry

`core` supports:

- **QoS 0** (at-most-once) and **QoS 1** (at-least-once),
- per-message metadata: `created_at`, optional `ttl`, `delivery_attempts`, `next_delivery_at`,
- periodic maintenance via `Broker::maintenance_tick`:
  - drops expired messages (TTL),
  - reschedules unacked QoS1 messages with exponential backoff,
  - stops after `max_retries`.

`blipmqd` runs the maintenance loop on a Tokio interval and respects graceful shutdown.

## Graceful shutdown

On `SIGINT`/`SIGTERM` (Ctrl+C), `blipmqd`:

1. marks the broker as shutting down (no new publishes accepted),
2. signals the network server and maintenance tasks to stop,
3. waits up to a bounded timeout for in-flight QoS1 messages to drain,
4. flushes the WAL, then exits.

This preserves at-least-once semantics across restarts without abrupt loss of in-flight messages.

## Development

### Tests

```bash
cargo test --workspace          # full suite (37 tests, ~1s)
cargo test -p core              # broker logic
cargo test -p net               # frame + connection + push delivery
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

Hot paths (publish, frame encode/decode, WAL append) are annotated with `#[inline(always)]` and tracing spans so `perf` and `tokio-console` can attribute time accurately. [`docs/DEVELOPER_GUIDE.md`](./docs/DEVELOPER_GUIDE.md) has more.

## Roadmap

The high-level phases are tracked in `bench_compare/results/`:

- **Phase 1 — net hot path** ✅ — push delivery, batched writer, TCP_NODELAY, zero-copy decode
- **Phase 2 — core fanout** ✅ — sharded subscriptions, snapshot fanout, AHash, shared push slot
- **Phase 3 — WAL durability** ✅ — group-commit fsync, segmented log, CRC over header
- **Phase 4 — recovery** ✅ — ack journal, fast replay, checkpoint snapshots, background segment compaction
- **Phase 5 — observability** ✅ (histograms + topic counters), in progress (more histogram surfaces)
- **Phase 6 — production hardening** (in progress) — NACK 503 on backpressure ✅, slow-consumer policy ✅, topic validation ✅, TLS pending, auth rate limiting pending
- **Phase 7 — feature unification** (in progress) — NATS-style subject wildcards `*` / `>` ✅, Kafka-style consumer groups + offset replay pending, RabbitMQ-style DLQ + per-message TTL pending

## License

Dual-licensed under [MIT](./LICENSE-MIT) or [Apache 2.0](./LICENSE-APACHE) at your option.
