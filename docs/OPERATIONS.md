# BlipMQ Operations Guide

This guide explains how to run the BlipMQ daemon (`blipmqd`), configure it, inspect metrics, and manage graceful shutdown and WAL behavior.

## Running the Daemon

From the workspace root:

```bash
# Debug build
cargo run -p blipmqd -- --config ./config/blipmq-dev.toml

# Release build
cargo build -p blipmqd --release
./target/release/blipmqd --config ./config/blipmq-dev.toml
```

`blipmqd` will:

- Load configuration from the specified file, with environment overrides.
- Initialize tracing (stdout logging or `tokio-console`).
- Open and replay the WAL.
- Start:
  - The TCP server (binary protocol).
  - The metrics HTTP server.
  - The maintenance scheduler.
- Block until a termination signal (e.g. Ctrl+C).

### CLI Flags

`blipmqd` uses `clap` and currently supports:

- `--config <path>`:
  - Optional path to a TOML or YAML config file.
  - If omitted, `BLIPMQ_CONFIG` can be used instead.

Example:

```bash
blipmqd --config /etc/blipmq/blipmq.toml
```

## Environment Variables

`blipmq_config` defines these environment variables (all optional):

- `BLIPMQ_CONFIG` – path to the config file (TOML or YAML).
- `BLIPMQ_BIND_ADDR` – overrides `bind_addr` (e.g. `0.0.0.0`).
- `BLIPMQ_PORT` – overrides `port`.
- `BLIPMQ_METRICS_ADDR` – overrides `metrics_addr`.
- `BLIPMQ_METRICS_PORT` – overrides `metrics_port`.
- `BLIPMQ_WAL_PATH` – overrides `wal_path`.
- `BLIPMQ_FSYNC_POLICY` – overrides `fsync_policy`.
- `BLIPMQ_MAX_RETRIES` – overrides `max_retries`.
- `BLIPMQ_RETRY_BACKOFF_MS` – overrides `retry_backoff_ms`.
- `BLIPMQ_ALLOWED_API_KEYS` – comma-separated list of API keys.
- `BLIPMQ_ENABLE_TOKIO_CONSOLE` – `"1"`, `"true"`, `"yes"`, or `"on"` to enable `tokio-console`.

These override file values when present.

## Config File Examples

### Minimal Development Config (TOML)

`config/blipmq-dev.toml`:

```toml
bind_addr = "127.0.0.1"
port = 5555

metrics_addr = "127.0.0.1"
metrics_port = 9100

wal_path = "data/blipmq-dev.wal"
# fsync_policy values:
#   "always"          -> fsync on every append
#   "none"            -> never fsync in the WAL layer
#   "every_n:64"      -> fsync after every 64 appends
#   "interval_ms:20"  -> fsync when 20 ms have elapsed
fsync_policy = "every_n:64"

max_retries = 3
retry_backoff_ms = 100

allowed_api_keys = ["dev-key-1"]

enable_tokio_console = false
```

### Env Overrides Example

```bash
export BLIPMQ_CONFIG=./config/blipmq-dev.toml

# Expose TCP & metrics on all interfaces
export BLIPMQ_BIND_ADDR=0.0.0.0
export BLIPMQ_METRICS_ADDR=0.0.0.0

# Place WAL on a fast local SSD
export BLIPMQ_WAL_PATH=/var/lib/blipmq/blipmq.wal
export BLIPMQ_FSYNC_POLICY="interval_ms:50"

# API keys for dev
export BLIPMQ_ALLOWED_API_KEYS="dev-key-1,dev-key-2"

blipmqd
```

## Metrics Endpoint

`blipmq_metrics` exposes a simple text metrics endpoint:

- Address: `metrics_addr:metrics_port` (e.g. `127.0.0.1:9100`).
- Path: `/metrics`.

Example:

```bash
curl http://127.0.0.1:9100/metrics
```

Typical response:

```text
topics 3
subscribers 5
messages_published_total 12345
messages_delivered_total 12001
messages_inflight 44
wal_appends_total 5678
wal_bytes_total 987654
```

Values are provided by:

- `Broker` (`topics`, `subscribers`, published/delivered counts, inflight).
- `WriteAheadLog` (`wal_appends_total`, `wal_bytes_total`).

## Graceful Shutdown & WAL Flush

`blipmqd` performs a coordinated shutdown when it receives a termination signal (e.g. Ctrl+C):

1. Wait for signal via `tokio::signal::ctrl_c()`.
2. Mark broker as shutting down:
   - `Broker::begin_shutdown()` prevents new publishes from being accepted.
3. Send `true` on a shutdown `watch` channel to:
   - Stop the network accept loop.
   - Stop the maintenance scheduler.
4. Wait for inflight QoS1 messages to drain:
   - Poll `Broker::inflight_message_count()`.
   - Wait up to a bounded timeout (e.g. 2 seconds), sleeping between checks.
5. Flush WAL:
   - `Broker::flush_wal()` calls `WriteAheadLog::flush()` and `sync_data()` as needed by `WalConfig`.
6. Log a summary and exit.

Operational implications:

- QoS1 in-flight messages are given a chance to complete and be ACKed.
- If messages remain in-flight after the timeout, they will be recovered from WAL and redelivered after restart (at-least-once).
- QoS0 messages may be lost during shutdown; they are not tracked or recovered.

## Docker Compose Example

Below is a full `docker-compose.yaml` example for running BlipMQ in a containerized environment.

### Example Dockerfile

`Dockerfile`:

```dockerfile
FROM rust:1.79 as builder

WORKDIR /app
COPY . .
RUN cargo build -p blipmqd --release

FROM debian:stable-slim

RUN useradd --system --no-create-home --shell /usr/sbin/nologin blipmq
WORKDIR /var/lib/blipmq

COPY --from=builder /app/target/release/blipmqd /usr/local/bin/blipmqd

RUN mkdir -p /etc/blipmq /var/lib/blipmq \
    && chown -R blipmq:blipmq /etc/blipmq /var/lib/blipmq

USER blipmq

ENTRYPOINT ["/usr/local/bin/blipmqd"]
CMD ["--config", "/etc/blipmq/blipmq.toml"]
```

### docker-compose.yaml

```yaml
version: "3.9"

services:
  blipmq:
    image: blipmq:latest
    container_name: blipmq
    restart: unless-stopped

    ports:
      - "5555:5555"   # broker TCP
      - "9100:9100"   # metrics HTTP

    volumes:
      - ./config/blipmq-prod.toml:/etc/blipmq/blipmq.toml:ro
      - ./data:/var/lib/blipmq

    environment:
      - RUST_LOG=info
      - BLIPMQ_ALLOWED_API_KEYS=prod-key-1
      - BLIPMQ_ENABLE_TOKIO_CONSOLE=false

    healthcheck:
      test: ["CMD", "curl", "-f", "http://127.0.0.1:9100/metrics"]
      interval: 10s
      timeout: 3s
      retries: 5
```

This setup:

- Runs `blipmqd` as a non-root user (`blipmq`).
- Uses a read-only config file mounted from the host.
- Persists WAL and durable state in `./data`.
- Exposes the broker TCP port and metrics HTTP port to the host.
