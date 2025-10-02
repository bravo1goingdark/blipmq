<p align="center">
  <img src="./assets/readme-banner.png" alt="BlipMQ Logo" width="1200" />
</p>

<p align="center">
  <b>BlipMQ</b> is an ultra-lightweight, fault-tolerant message queue written in Rust — built for edge, embedded, and developer-first environments.
</p>

<p align="center">
  ⚡ <i>“Kafka-level durability. MQTT-level simplicity. NATS-level performance — all in one binary.”</i>
</p>

<p align="center">
  📖 <a href="https://bravo1goingdark.github.io/blipmq-site/blipmq/docs"><strong>Documentation</strong></a> ·
  🚀 <a href="#-quick-start"><strong>Quick Start</strong></a> ·
  📦 <a href="https://github.com/bravo1goingdark/blipmq/releases"><strong>Releases</strong></a> ·
  💬 <a href="https://github.com/bravo1goingdark/blipmq/discussions"><strong>Discussions</strong></a>
</p>

## 📢 Follow Us

<p align="center">
  <a href="https://blipmq.dev">
    <img src="https://img.shields.io/badge/Website-blipmq.dev-0A0A0A?style=flat&logo=google-chrome&logoColor=white" alt="Website" />
  </a>
  <a href="https://x.com/blipmq">
    <img src="https://img.shields.io/badge/Twitter-@blipmq-1DA1F2?style=flat&logo=twitter&logoColor=white" alt="Twitter" />
  </a>
  <a href="https://linkedin.com/company/blipmq">
    <img src="https://img.shields.io/badge/LinkedIn-blipmq-blue?style=flat&logo=linkedin&logoColor=white" alt="LinkedIn" />
  </a>
  <a href="https://www.instagram.com/blipmq">
    <img src="https://img.shields.io/badge/Instagram-@blipmq-E4405F?style=flat&logo=instagram&logoColor=white" alt="Instagram" />
  </a>
</p>


## 🧹 Features — `v1.0.0`

✅ = Implemented in `v1.0.0`
⬜ = Planned for future

### 🔌 Core Broker

* ✅ Single static binary (no runtime deps)
* ✅ TCP-based FlatBuffers protocol
* ✅ Topic-based publish/subscribe
* ✅ QoS 0 delivery
* ⬜ QoS 1 support
* ✅ Per-subscriber isolated in-memory queues
* ✅ Configurable TTL and max queue size
* ✅ Overflow policies: `drop_oldest`, `drop_new`, `block`

### 🔐 Durability & Safety

* ✅ Append-only Write-Ahead Log (WAL)
* ✅ WAL segmentation (rotated files)
* ✅ Replay unacknowledged messages on restart
* ✅ CRC32 checksum for corruption detection
* ✅ Batched WAL flushing with fsync

### 📈 Observability

* ✅ Prometheus `/metrics` endpoint
* ✅ Tracing + structured logs
* ✅ Connection + delivery stats

### 🧰 Operational Controls

* ✅ Configurable limits (connections, queue depth)
* ✅ API-key based authentication

---

## 💡 Ideal Use Cases

| Scenario                              | Why BlipMQ?                                                                 |
|---------------------------------------|-----------------------------------------------------------------------------|
| 🚁 **IoT or Edge Gateways**           | Single-binary broker with ultra-low latency and no heavy runtime overhead   |
| 🧪 **Local Testing/Dev Environments** | Embedded broker with fast crash recovery and zero-setup simplicity          |
| ⚙️ **Internal Microservice Bus**      | In-process message bus with no ops burden and blazing-fast pub/sub          |
| 🧱 **CI/CD Pipelines**                | Durable and high-speed event ingestion for test runners or deployments      |
| 📜 **Lightweight Log Ingestion**      | Fire-and-forget logging with fan-out support and minimal latency            |
| 📊 **Metrics and Telemetry Streams**  | Real-time time-series ingestion with optional per-subscriber filtering      |
| 💬 **Real-Time Chat Infrastructure**  | Ordered QoS 0 messaging with low latency, ideal for user/topic message flow |
| 🕹️ **Multiplayer Game Messaging**    | Per-player queues with optional reliability and sub-ms delivery             |
| 📦 **Local Job Queues**               | Drop-in replacement for Redis queues with retry and durability options      |
| 🧠 **Distributed Cache Invalidation** | High-speed fan-out to multiple replicas or edge caches                      |
| 🪄 **Feature Flag Propagation**       | Push config and toggles without polling                                     |
| 🔌 **Serverless & WASM Runtimes**     | Runs inside Wasm or serverless apps due to its Rust-native, no-deps design  |
| 📡 **Offline-First Apps**             | Queue and sync later with pluggable durability and buffering                |
| 🎮 **In-Process Game/Sim Engines**    | Internal coordination across subsystems (AI, UI, Physics)                   |
| 🧬 **Model Serving / Hot Reload**     | Real-time model or config updates to downstream ML inference nodes          |
| 🧯 **Alert Fan-out (Security/Ops)**   | Instantly notify multiple alerting sinks from a single producer             |
| 🧰 **Embedded/Edge Command Systems**  | Low memory, high-speed messaging for control signals                        |
| 🔄 **Change Data Capture (CDC)**      | Real-time DB change fan-out with replay capability                          |
| 📤 **Webhook / Event Fan-out**        | Ingest events and deliver to many consumer systems/webhooks                 |
| 🦡 **Frontend WebSocket Fan-out**     | Backend-to-browser pub/sub with per-topic filtering                         |

---

## 🚀 When to Pick BlipMQ?

| If you need…                        | Why BlipMQ is ideal                                                   |
|-------------------------------------|-----------------------------------------------------------------------|
| 🩾 Minimal resource usage           | Binary < 5MB, no JVM, GC, or heavy dependencies                       |
| ⚡ Ultra-low latency                 | Sub-millisecond paths with in-process and lock-free delivery          |
| 🧰 Simple deployment                | No infrastructure, just `cargo add blipmq` or run a single binary     |
| 🔄 Pluggable QoS                    | Choose between fire-and-forget (QoS 0) or guaranteed (QoS 1) delivery |
| 💬 Per-subscriber queues            | Each subscriber gets its own delivery queue                           |
| 🪶 Lightweight alternative to Kafka | When Kafka or NATS is overkill for internal or local communication    |
| 🧠 Full internal control            | Built for embedding, hacking, and customizing from your app           |

---

## ⚙️ Quick Start

- Build (Linux recommended):
  - `cargo build --release --features mimalloc`
- Start broker:
  - `cargo run --release --bin blipmq -- start --config blipmq.toml`
- Subscribe with CLI:
  - `cargo run --release --bin blipmq-cli -- --addr 127.0.0.1:7878 sub chat`
- Publish a message (with TTL in ms):
  - `cargo run --release --bin blipmq-cli -- --addr 127.0.0.1:7878 pub chat --ttl 5000 "hello world"`
- Publish a file:
  - `cargo run --release --bin blipmq-cli -- --addr 127.0.0.1:7878 pubfile chat --ttl 0 ./message.bin`
- Publish from stdin:
  - `echo "streamed data" | cargo run --release --bin blipmq-cli -- --addr 127.0.0.1:7878 pub chat --ttl 0 -`
- Subscribe and stop after N messages:
  - `cargo run --release --bin blipmq-cli -- --addr 127.0.0.1:7878 sub chat --count 10`

## 🧭 CLI Reference

- `pub <topic> [--ttl <ms>] <message | ->`
  - Sends a message to a topic. Use `-` to read from STDIN.
- `pubfile <topic> [--ttl <ms>] <path>`
  - Sends the contents of a file (binary safe).
- `sub <topic> [--count N]`
  - Subscribes to a topic. Optionally exits after N messages.
- `unsub <topic>`
  - Unsubscribes from a topic.

All sizes are validated against `server.max_message_size_bytes` from `blipmq.toml`.

## 🧩 Configuration (blipmq.toml)

Minimal example:

```
[server]
bind_addr = "127.0.0.1:7878"
max_connections = 256
max_message_size_bytes = 1048576

[metrics]
bind_addr = "127.0.0.1:9090"

[queues]
topic_capacity = 1024
subscriber_capacity = 512
overflow_policy = "drop_oldest"

[delivery]
max_batch = 64
max_batch_bytes = 262144 # 256 KiB
flush_interval_ms = 1
fanout_shards = 0 # auto
default_ttl_ms = 0
```

## 📊 Metrics

A minimal HTTP server exposes counters on `metrics.bind_addr` (plain text):
- blipmq_published
- blipmq_enqueued
- blipmq_dropped_ttl
- blipmq_dropped_sub_queue_full
- blipmq_flush_bytes
- blipmq_flush_batches

## 🏎️ Performance Notes

- Fast hashing for shard selection via `ahash` (AHasher) on hot paths.
- Bounded queues end-to-end (`flume` for topics, lock-free `ArrayQueue` for subscribers) to apply backpressure.
- Per-connection writer task batches frames and uses a reactive timer that only fires when there is pending data.
- Nagle’s algorithm disabled (TCP_NODELAY) for low-latency flushes.
- Release profile uses `lto = true`, `codegen-units = 1`, `panic = "abort"`.
- Optional allocator: build with `--features mimalloc` (works on Linux).
- For maximum throughput on your CPU, you can use `RUSTFLAGS="-C target-cpu=native"`.

## 🌀 Reactive by Design (No Polling)

- All network reads/writes are async and await readiness.
- Topic fanout workers block on async channels; no spin loops.
- Subscriber flush loops are wake-driven via `Notify` and drain until empty; no busy waiting when idle.
- The writer’s time-based flush is gated by buffer non-emptiness to avoid periodic wakeups with no data.

---

> 📄 Licensed under BSD-3-Clause — see [LICENSE](./LICENSE)
