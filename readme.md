<p align="center">
  <img src="./assets/readme-banner.png" alt="BlipMQ Logo" width="1200" />
</p>

<p align="center">
  <b>BlipMQ</b> is a lightweight, high-performance message broker written in Rust, designed for durable pub/sub with QoS, WAL, and simple operational ergonomics.
</p>

<p align="center">
  <a href="https://blipmq.dev"><strong>Website</strong></a> ·
  <a href="https://github.com/bravo1goingdark/blipmq"><strong>GitHub</strong></a> ·
  <a href="https://x.com/blipmq"><strong>Twitter</strong></a> ·
  <a href="https://linkedin.com/company/blipmq"><strong>LinkedIn</strong></a>
</p>

# BlipMQ (workspace)

This repository contains the Rust workspace used to build and evolve the core broker, daemon, and supporting crates.

## Workspace layout

- `blipmqd` – main broker daemon (TCP protocol, WAL, auth, metrics)
- `blipmq_core` – core broker logic (topics, queues, QoS, TTL, retry)
- `blipmq_net` – TCP server and binary framing
- `blipmq_wal` – write-ahead log (append-only, CRC-checked, indexable)
- `blipmq_auth` – static API key authentication
- `blipmq_metrics` – HTTP metrics endpoint
- `blipmq_config` – configuration loader (TOML/YAML + env merge)
- `blipmq_bench` – in-process performance benchmark harness

## Running the daemon from source

From the workspace root:

```bash
cargo run -p blipmqd -- --config ./config/blipmq-dev.toml
```

You can also rely on the `BLIPMQ_CONFIG` environment variable instead of `--config`.

## Configuration

`blipmqd` uses `blipmq_config` to load configuration from:

- an optional TOML or YAML file, and
- environment variables that override file defaults.

Example minimal TOML config:

```toml
bind_addr = "127.0.0.1"
port = 7878

metrics_addr = "127.0.0.1"
metrics_port = 9090

wal_path = "blipmq.wal"
fsync_policy = "every_n:1"        # or "always", "none", "interval_ms:50"

max_retries = 3
retry_backoff_ms = 100

allowed_api_keys = ["dev-key-1", "dev-key-2"]
```

Environment overrides (examples):

- `BLIPMQ_BIND_ADDR`, `BLIPMQ_PORT`
- `BLIPMQ_METRICS_ADDR`, `BLIPMQ_METRICS_PORT`
- `BLIPMQ_WAL_PATH`, `BLIPMQ_FSYNC_POLICY`
- `BLIPMQ_MAX_RETRIES`, `BLIPMQ_RETRY_BACKOFF_MS`
- `BLIPMQ_ALLOWED_API_KEYS="key1,key2,..."`

## Metrics endpoint

`blipmq_metrics` exposes a simple HTTP endpoint suitable for Prometheus scraping or basic monitoring:

- Path: `GET /metrics` on `metrics_addr:metrics_port`
- Response (plain text), for example:

```text
topics <n>
subscribers <n>
messages_published_total <n>
messages_delivered_total <n>
messages_inflight <n>
wal_appends_total <n>
wal_bytes_total <n>
```

These values are provided by `blipmq_core::Broker` and `blipmq_wal::WriteAheadLog`.

## WAL and durability

`blipmq_wal` implements an append-only log with:

- fixed header and `[id][len][crc32][payload...]` record layout,
- CRC32 validation for corruption detection,
- in-memory id→offset index with rebuild on startup,
- configurable fsync policy via `WalConfig`:
  - `fsync_every_n`,
  - `fsync_interval`.

`blipmqd` uses the WAL to:

- durably record QoS1 publishes before fan-out, and
- replay messages on startup to recover durable topics and queues.

## QoS, TTL, and retry

`blipmq_core` supports:

- QoS 0 (at-most-once) and QoS 1 (at-least-once),
- per-message metadata:
  - `created_at`,
  - optional TTL,
  - `delivery_attempts`,
  - `next_delivery_at`,
- periodic maintenance:
  - drops expired messages (TTL),
  - reschedules unacked QoS1 messages with exponential backoff,
  - stops after a configurable `max_retries`.

`blipmqd` runs a Tokio-based maintenance loop that calls `Broker::maintenance_tick` on an interval and respects graceful shutdown.

## Graceful shutdown

On SIGINT/SIGTERM (Ctrl+C), `blipmqd`:

- marks the broker as shutting down (no new publishes),
- signals the network server and maintenance tasks to stop,
- waits up to a bounded timeout for in-flight QoS1 messages to drain,
- flushes the WAL, then exits.

This provides at-least-once semantics for QoS1 across restarts while avoiding abrupt loss of in-flight messages during orderly shutdowns.

## Benchmark harness (`blipmq_bench`)

The `blipmq_bench` crate provides an in-process benchmark that exercises the `Broker` API directly (with optional WAL) to measure basic throughput and latency:

```bash
cargo run -p blipmq_bench -- \
  --publishers 4 \
  --subscribers 4 \
  --topics 8 \
  --messages 50000 \
  --msg-size 256 \
  --qos 1 \
  --wal
```

Options:

- `--publishers` – number of concurrent publisher tasks
- `--subscribers` – number of subscriber tasks
- `--topics` – number of topics used in round-robin
- `--messages` – messages per publisher
- `--msg-size` – payload size in bytes (must be ≥ 8)
- `--qos` – QoS mode (`0` = at-most-once, `1` = at-least-once)
- `--wal` – enable durable publishes via WAL

The harness reports:

- total messages received,
- throughput (msgs/sec),
- p50 / p95 / p99 latency (µs).

It is intended as a sanity-check for high-performance behavior rather than a formal benchmark suite.

## Profiling and flamegraphs

You can profile `blipmqd` using standard Linux tools and `flamegraph`:

```bash
cargo install flamegraph

# Build a release binary
cargo build -p blipmqd --release

# Record perf data (may require sudo)
sudo perf record -F 99 -g -- target/release/blipmqd -- --config ./config/blipmq-dev.toml

# Generate a flamegraph
sudo flamegraph
```

Hot paths such as publish, frame encode/decode, and WAL append are annotated with `#[inline(always)]` and tracing spans so that perf and `tokio-console` can attribute time accurately.
