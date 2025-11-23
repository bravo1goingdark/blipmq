# BlipMQ Performance Guide

This document covers expected performance characteristics, tuning knobs, and how to use the BlipMQ benchmark harness.

## Expected Throughput & Latency (Indicative)

The numbers below are indicative ranges on a modern 8-core x86 server with SSD storage and local clients. Actual numbers depend on hardware, workload, and configuration.

- **QoS0 in-memory (no WAL)**:
  - Throughput: 1–5 million msgs/sec (small messages).
  - Latency: p50 < 200µs, p99 < 1ms.
- **QoS1 in-memory (no WAL)**:
  - Throughput: 0.5–2 million msgs/sec.
  - Latency: higher due to inflight tracking and ACK handling.
- **QoS1 durable (WAL on)**:
  - Throughput: 100k–500k msgs/sec with `fsync_every_n=64` or `interval_ms=20`.
  - Latency: p50 around 500µs–2ms; tail latency influenced by fsync behavior.
- **Scaled publishers/subscribers**:
  - Scales with number of CPU cores and topics.
  - Contention primarily in:
    - WAL append path.
    - Per-subscriber queues under heavy fan-out.
    - Network stack for many small frames.

These are not guarantees but target ranges for tuning.

## Tuning Knobs

### WAL Fsync Behavior

Controlled via `Config.fsync_policy`, interpreted by `blipmqd::make_wal_config`:

- `"always"`:
  - fsync on every append.
  - Highest durability, lowest throughput.
- `"none"`:
  - No fsync in the WAL layer.
  - Highest throughput, risk of losing recent records on crash.
- `"every_n:<n>"` (e.g. `"every_n:64"`):
  - fsync after every `n` records.
- `"interval_ms:<ms>"` (e.g. `"interval_ms:20"`):
  - fsync when `<ms>` milliseconds have elapsed since the last fsync.

Choosing a policy:

- For strict durability: `"every_n:1"` (equivalent to `"always"`) or small `n`.
- For balanced: `"every_n:64"` or `"interval_ms:20"`.
- For maximum throughput with some risk: `"none"` plus external redundancy.

### Topic Sharding

`Broker` uses sharded topic maps:

- `TopicShards::new(16)` creates 16 shards.
- Each shard: `RwLock<HashMap<TopicName, Arc<Topic>>>`.
- Topics are assigned by hash of `TopicName`.

Implications:

- More topics ? better distribution across shards.
- Single hot topic ? more contention on that topic’s shard.
  - Consider using multiple topics per logical stream to spread load.

### Per-Subscriber Queue Capacity

`BrokerConfig.per_subscriber_queue_capacity`:

- Upper bound for `pending + inflight` messages per subscription.
- When capacity is reached, oldest pending messages are dropped on enqueue.
- QoS1 messages are durable via WAL, but may not be held in memory indefinitely.

Tuning:

- Low capacity (e.g. 128–1024):
  - Reduces memory footprint.
  - More aggressive dropping when subscribers are slow.
- High capacity:
  - Better buffering but more memory usage and potential latency.

### QoS1 Retry & Backoff

Controlled by:

- `Config.max_retries`:
  - Maximum number of delivery attempts.
- `Config.retry_backoff_ms`:
  - Base backoff in milliseconds.

Effective behavior:

- `SubscriberQueue::dequeue` computes `next_delivery_at` using exponential backoff based on attempt count and base delay.
- `Broker::maintenance_tick` moves eligible inflight messages back into `pending` when `now >= next_delivery_at`.

Guidance:

- For low-latency, highly available consumers:
  - `max_retries` can be higher (e.g. 10).
  - `retry_backoff_ms` can be small (e.g. 10–20).
- For resource-constrained or bursty workloads:
  - Use moderate `max_retries` (e.g. 3–5).
  - Larger backoffs (e.g. 100–500ms) to avoid overload during outages.

### Networking Considerations

- Use low-latency networks (local or data center).
- Tune OS TCP parameters for high throughput:
  - `net.core.rmem_max`, `net.core.wmem_max`, etc.
- Co-locate publishers/subscribers with broker where possible to minimize RTT.
- Batch operations:
  - Pipeline multiple `PUBLISH` or `POLL` frames before waiting on responses.

## Benchmark Harness (`bench`)

`bench` provides a full-protocol performance harness. It:

- Spawns an in-process BlipMQ instance (including WAL when configured).
- Starts TCP clients that speak the real protocol.
- Measures throughput and latency.

### Modes

Controlled by `--mode`:

- `qos0`:
  - QoS0 in-memory benchmark.
- `qos1`:
  - QoS1 in-memory benchmark.
- `durable`:
  - QoS1 with WAL enabled.
- `scaled`:
  - Emphasizes scaled publishers/subscribers.

### CLI Options

From `bench::Cli`:

- `--mode <qos0|qos1|durable|scaled>`:
  - Benchmark mode (default: `qos1`).
- `--publishers <N>`:
  - Number of publisher clients (default: 1).
- `--subscribers <M>`:
  - Number of subscriber clients (default: 1).
- `--message-size <bytes>`:
  - Payload size in bytes, must be = 8 (default: 256).
- `--qos <0|1>`:
  - QoS mode (default: 1). Some modes override this.
- `--use-wal <on|off>`:
  - Enable WAL (default: `off`). Some modes override this.
- `--duration-secs <seconds>`:
  - Duration to run; if `0`, only `total-messages` is used (default: 10).
- `--total-messages <count>`:
  - Total messages to publish across all publishers; if `0`, only `duration-secs` is used (default: 0).

### Example Benchmark Command

QoS1 durable benchmark:

```bash
cargo run -p bench -- \
  --mode durable \
  --publishers 4 \
  --subscribers 4 \
  --message-size 512 \
  --qos 1 \
  --use-wal on \
  --duration-secs 30
```

QoS0 in-memory benchmark:

```bash
cargo run -p bench -- \
  --mode qos0 \
  --publishers 8 \
  --subscribers 8 \
  --message-size 256 \
  --qos 0 \
  --use-wal off \
  --duration-secs 15
```

### Sample Output

Example output from the durable QoS1 mode:

```text
=== BlipMQ bench results ===
publishers: 4
subscribers: 4
qos: AtLeastOnce
use_wal: true
message_size: 512 bytes
messages received: 100000
elapsed: 5.002s
throughput: 19992 msgs/sec
p50 latency: 850.00 µs
p95 latency: 1500.00 µs
p99 latency: 3200.00 µs
```

Notes:

- Latencies are calculated from timestamps embedded in the first 8 bytes of each message payload.
- Throughput is total received messages divided by wall-clock time.
- The harness uses the real TCP protocol and `net` framing, not a mocked broker.

## Hardware Recommendations

For best performance:

- **CPU**:
  - At least 4–8 cores at modern frequencies (= 3 GHz).
  - Higher core counts help with many topics/subscribers.
- **Memory**:
  - Enough RAM to hold in-memory queues and WAL buffers without swapping.
  - Monitor memory usage if `per_subscriber_queue_capacity` and subscriber count are large.
- **Storage**:
  - Use NVMe SSDs for WAL for best durability/latency tradeoff.
  - Avoid spinning disks for workloads that rely heavily on WAL.
- **Network**:
  - Low-latency, high-bandwidth network interfaces.
  - Co-locate broker and clients when possible for lowest latency.

## Profiling & tokio-console

For more detailed performance analysis:

- Enable `tokio-console` via config:

  ```toml
  enable_tokio_console = true
  ```

- Run `tokio-console` in a separate terminal and point it to the running process (see `console-subscriber` docs).

CPU profiling with `perf` and `flamegraph`:

```bash
cargo install flamegraph

# Build release binary
cargo build -p blipmqd --release

# Record perf data (may require sudo)
sudo perf record -F 99 -g -- target/release/blipmqd -- --config ./config/blipmq-dev.toml

# Generate flamegraph.svg
sudo flamegraph
```

Hot paths such as publish, frame encode/decode, and WAL append are annotated with `#[inline(always)]` and tracing spans for clear attribution in profiles.

