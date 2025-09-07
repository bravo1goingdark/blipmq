# WARP.md

This file provides guidance to WARP (warp.dev) when working with code in this repository.

Project purpose
- BlipMQ is a lightweight message broker library plus binaries (broker and CLI) in Rust. It uses a FlatBuffers-framed TCP protocol with topic-based pub/sub and per-subscriber queues.

Common commands
- Build
  - Debug
    ```bash path=null start=null
    cargo build
    ```
  - Release
    ```bash path=null start=null
    cargo build --release
    ```
- Lint and format (matches CI)
  ```bash path=null start=null
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
  ```
  - To format locally:
    ```bash path=null start=null
    cargo fmt
    ```
- Test
  - Run all tests (with output):
    ```bash path=null start=null
    cargo test --all -- --nocapture
    ```
  - Run a single test by name filter:
    ```bash path=null start=null
    cargo test topic_fanout
    ```
- Binaries
  - Start the broker (uses default-run = "blipmq"):
    ```bash path=null start=null
    cargo run -- start --config blipmq.toml
    ```
  - Override config path via environment variable:
    - macOS/Linux
      ```bash path=null start=null
      BLIPMQ_CONFIG=path/to/your.toml cargo run -- start
      ```
    - Windows (PowerShell)
      ```powershell path=null start=null
      $env:BLIPMQ_CONFIG = "path\to\your.toml"; cargo run -- start
      ```
  - Interactive client using the same binary:
    ```bash path=null start=null
    cargo run -- connect 127.0.0.1:8080
    ```
    Once connected, REPL commands: pub, sub, unsub, exit.
  - Dedicated CLI binary:
    ```bash path=null start=null
    cargo run --bin blipmq-cli -- --addr 127.0.0.1:8080 sub chat
    cargo run --bin blipmq-cli -- --addr 127.0.0.1:8080 pub chat --ttl 5000 "hello"
    cargo run --bin blipmq-cli -- --addr 127.0.0.1:8080 unsub chat
    ```
- Benchmarks
  ```bash path=null start=null
  cargo bench --bench network_benchmark
  ```
- FlatBuffers schema changes
  - Edit src/core/fbs/blipmq.fbs, then rebuild; build.rs auto-regenerates code.
  - To force regeneration:
    ```bash path=null start=null
    cargo clean -p blipmq && cargo build
    ```

Architecture and code structure (big picture)
- Crate layout
  - Library crate exports public modules and re-exports convenience entry points.
    - src/lib.rs re-exports:
      - start_broker (broker::engine::serve)
      - load_config, Config (config)
  - Binaries
    - src/bin/blipmq.rs: multipurpose binary with two subcommands
      - start: loads config, starts broker
      - connect: interactive REPL client (pub/sub/unsub)
    - src/bin/blipmq-cli.rs: dedicated CLI with subcommands Pub/Sub/Unsub
- Configuration
  - src/config/mod.rs defines strongly-typed Config and sub-structs (ServerConfig, QueueConfig, WalConfig, MetricsConfig, DeliveryConfig, AuthConfig).
  - CONFIG is a global Lazy<Config> loaded from blipmq.toml in the process working directory; load_config(path) is provided for explicit paths.
  - Key effects
    - server.bind_addr drives broker listen address
    - queues.topic_capacity sets the bounded flume capacity per topic
    - queues.subscriber_capacity sets per-subscriber queue capacity
    - queues.overflow_policy controls QoS0 overflow behavior (drop_oldest/drop_new)
    - delivery.max_batch controls per-flush coalescing; delivery.default_ttl_ms is the default message TTL
    - server.max_message_size_bytes constrains incoming frame sizes
- Protocol framing (FlatBuffers)
  - Build-time: build.rs compiles src/core/fbs/blipmq.fbs via flatbuffers-build (vendored); generated code is included with include!(concat!(env!("OUT_DIR"), "/flatbuffers/mod.rs")).
  - Client -> Server: length-prefixed ClientCommand (Action, topic, payload, ttl_ms).
  - Server -> Client: length-prefixed frames with a 1-byte type tag, followed by a FlatBuffers payload.
    - ServerFrame::SubAck(topic, info)
    - ServerFrame::Message(id, payload, timestamp, ttl_ms)
- Broker engine (async I/O)
  - src/broker/engine/server.rs
    - serve(): binds to CONFIG.server.bind_addr, accepts clients, spawns a task per connection.
    - Each client task reads 4-byte length + buffer; decode_command() maps FlatBuffers to ClientCommand.
    - Actions
      - Pub: TopicRegistry.get_topic(topic)?.publish(new_message_with_ttl(payload, ttl_ms)).
      - Sub: send SubAck, register Subscriber with per-subscriber capacity from config.
      - Unsub: remove subscriber from topic.
      - Quit: disconnect.
    - Safety limits: rejects frames larger than server.max_message_size_bytes.
- Topics and fanout
  - src/core/topics
    - TopicRegistry: DashMap-backed registry; create_or_get_topic uses queues.topic_capacity from CONFIG.
    - Topic: holds subscribers and a bounded flume::Sender for input. A dedicated task fans out each message concurrently to subscribers using FuturesUnordered.
- Subscribers and delivery
  - src/core/subscriber
    - Subscriber: per-connection queue (QoS0) + Notify + background flush task.
    - Flush loop drains up to delivery.max_batch, skips expired messages (TTL), encodes frames, and writes via a shared BufWriter guarded by Mutex.
- Queues (QoS 0)
  - src/core/queue/qos0.rs: crossbeam_queue::ArrayQueue-based; overflow policy DropNew or DropOldest.
  - QoS1 is scaffolded (src/core/queue/qos1.rs) but not yet implemented.
- Messages and commands
  - src/core/command.rs: create/encode/decode ClientCommand, plus convenience constructors new_pub_with_ttl/new_sub/new_unsub.
  - src/core/message.rs: Message struct, encode/decode, ServerFrame encode/decode, and TTL-aware constructors.
- Logging
  - src/logging/mod.rs: tracing_subscriber setup with env filter; used by tests and binaries to initialize logging.

CI alignment (from .github/workflows/rust.yml)
- Toolchain: stable
- Steps run in CI (mirror locally when needed):
  ```bash path=null start=null
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
  cargo test --all -- --nocapture
  ```

Configuration file (blipmq.toml)
- Expected in the working directory when running the broker/CLI.
- Notable keys (values shown are examples; redact secrets):
  ```toml path=null start=null
  [server]
  bind_addr = "127.0.0.1:8080"
  max_connections = 100
  max_message_size_bytes = 1048576

  [auth]
  api_keys = ["<redacted>"]

  [queues]
  topic_capacity = 1000
  subscriber_capacity = 512
  overflow_policy = "drop_oldest"

  [delivery]
  max_batch = 64
  default_ttl_ms = 60000
  ```

Notes for future work (orientation)
- QoS1 scaffolding exists but is not yet implemented; current delivery path is QoS0-only with per-subscriber bounded queues.
- Modifying src/core/fbs/blipmq.fbs changes the on-the-wire schema; rebuild to regenerate generated code.

