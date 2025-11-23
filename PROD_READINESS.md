# BlipMQ Production Readiness Checklist

This checklist must be completed before any BlipMQ release is declared production-ready.

Each item should be validated on the target environment (or a close staging replica) and documented with test results.

---

## 1. Reliability tests

- [ ] **QoS1 delivery semantics**
  - [ ] Verify that a QoS1 message is delivered at least once to every subscribed consumer.
  - [ ] Verify that acknowledging a QoS1 message removes it from the in-flight set and it is not redelivered during the same process lifetime.
- [ ] **QoS0 delivery semantics**
  - [ ] Verify that QoS0 messages are never retried or redelivered after being dequeued.
- [ ] **End-to-end tests under load**
  - [ ] Run a long-lived workload (≥ 1 hour) with mixed QoS0/QoS1 traffic, varying message sizes and topics, and confirm:
    - [ ] No panics or process crashes.
    - [ ] No unbounded growth in memory usage.
    - [ ] No unbounded growth in open file descriptors or TCP connections.
- [ ] **Backlog handling**
  - [ ] Fill in-memory queues close to configured capacities for multiple topics and verify:
    - [ ] Oldest messages are dropped according to capacity policies.
    - [ ] The process remains responsive and does not OOM.

---

## 2. Performance benchmarks

- [ ] **Baseline throughput**
  - [ ] Use `bench` in `qos0` mode to measure in-memory QoS0 throughput:
    - [ ] Record msgs/sec and p50/p95/p99 latencies for representative message sizes (e.g., 128B, 1KB, 4KB).
- [ ] **QoS1 performance**
  - [ ] Use `bench` in `qos1` mode (without WAL) to measure QoS1 throughput and latency:
    - [ ] Confirm that p95/p99 latency remains within acceptable SLOs under expected peak load.
- [ ] **Durable (WAL) performance**
  - [ ] Use `bench` in `durable` mode to measure throughput and latency with WAL enabled:
    - [ ] Measure impact of different `fsync_policy` settings (`always`, `every_n`, `interval_ms`) and choose a default per environment.
- [ ] **Scaled publishers/subscribers**
  - [ ] Use `bench` in `scaled` mode with realistic publisher/subscriber counts:
    - [ ] Confirm CPU utilization and latency remain within acceptable levels.
- [ ] **Profiling**
  - [ ] Capture CPU profiles (e.g., with `perf` + `flamegraph`) in at least:
    - [ ] QoS0 in-memory scenario.
    - [ ] QoS1 durable scenario.
  - [ ] Verify that hot paths (publish, framing, WAL append) show expected behavior and no unnecessary allocations or locks.

---

## 3. Chaos testing scenarios

- [ ] **Crash recovery**
  - [ ] Run scenario:
    - [ ] Publish a batch of QoS1 messages to durable topics.
    - [ ] Kill the broker abruptly (SIGKILL or process crash) while some messages are unacked.
    - [ ] Restart broker with the same WAL.
    - [ ] Confirm:
      - [ ] All unacked messages are redelivered at least once.
      - [ ] No duplicate durable messages beyond acceptable at-least-once semantics.
- [ ] **Random network disconnects**
  - [ ] Use a chaos tool (e.g., `chaos::maybe_disconnect` or tc/netem) to randomly close client connections:
    - [ ] Confirm broker remains stable and resources (connections, memory) are cleaned up.
- [ ] **Slow disk simulation**
  - [ ] Introduce artificial delay into WAL writes (e.g., `simulate_slow_disk_write` or slower storage):
    - [ ] Confirm backpressure is applied (producers slow down) rather than unbounded buffering.
- [ ] **WAL corruption injection**
  - [ ] Corrupt WAL files (e.g., using `corrupt_file_tail_bit` or direct byte flips):
    - [ ] Restart broker and confirm:
      - [ ] Corrupted segments are detected and reported via logs and metrics.
      - [ ] Broker does not panic; it either truncates or refuses the corrupted WAL gracefully.

---

## 4. Configuration validation

- [ ] **Config file and env overrides**
  - [ ] Verify that all relevant configuration fields are populated from TOML/YAML and can be overridden via environment variables:
    - [ ] `bind_addr`, `port`
    - [ ] `metrics_addr`, `metrics_port`
    - [ ] `wal_path`, `fsync_policy`
    - [ ] `max_retries`, `retry_backoff_ms`
    - [ ] `allowed_api_keys`
    - [ ] `enable_tokio_console`
- [ ] **Invalid config handling**
  - [ ] Provide malformed or incomplete configs and confirm:
    - [ ] Clear error messages are logged.
    - [ ] Process exits with non-zero code and does not start partially configured.
- [ ] **Runtime reconfiguration**
  - [ ] If config reload is supported (or planned), ensure:
    - [ ] Sensitive changes (e.g., WAL path, disk paths) either require restart or are handled with well-defined semantics.

---

## 5. Logging & metrics correctness

- [ ] **Log levels**
  - [ ] Confirm:
    - [ ] Normal operation produces limited `info` logs.
    - [ ] Errors (I/O failures, WAL corruption, config parse failures) are logged at `error` with context.
    - [ ] Debug/trace logs can be enabled via `RUST_LOG` without recompilation.
- [ ] **Metrics endpoint**
  - [ ] Confirm `/metrics` exposes:
    - [ ] `topics`
    - [ ] `subscribers`
    - [ ] `messages_published_total`
    - [ ] `messages_delivered_total`
    - [ ] `messages_inflight`
    - [ ] `wal_appends_total`
    - [ ] `wal_bytes_total`
  - [ ] Verify metrics values change as expected under test load.
  - [ ] Integrate with the production metrics system (e.g., Prometheus) and validate scrape configuration.

---

## 6. Security review

- [ ] **Authentication**
  - [ ] Ensure API key authentication is enforced on all client connections:
    - [ ] No non-HELLO/AUTH frames are processed before successful AUTH.
    - [ ] Invalid or missing API keys result in NACK and connection denial.
- [ ] **Configuration of secrets**
  - [ ] Confirm that API keys are not logged in plaintext.
  - [ ] Confirm that keys can be supplied via environment variables or secrets management and not hardcoded.
- [ ] **Network exposure**
  - [ ] Confirm `bind_addr` and port configuration restricts the broker to the intended interfaces.
  - [ ] Validate TLS termination strategy (if applicable) in front of BlipMQ.
- [ ] **Dependency and crate audit**
  - [ ] Run `cargo audit` and address critical vulnerabilities in dependencies.

---

## 7. Graceful shutdown

- [ ] **Signal handling**
  - [ ] Verify that `SIGINT`/`SIGTERM` are handled:
    - [ ] Broker stops accepting new connections and new publishes.
    - [ ] Background tasks (maintenance, metrics, network server) receive shutdown signals.
- [ ] **In-flight message draining**
  - [ ] Confirm that on shutdown:
    - [ ] The broker waits up to a bounded timeout for in-flight QoS1 messages to be acknowledged.
    - [ ] Unacked QoS1 messages are left in WAL for redelivery after restart.
- [ ] **WAL flush**
  - [ ] Confirm that WAL is flushed (fsync) before exit when configured to do so, and that no committed messages are lost across a clean shutdown.

---

## 8. Resource limits

- [ ] **CPU**
  - [ ] Verify that under expected peak load:
    - [ ] CPU utilization remains within capacity on target hardware.
    - [ ] No single-core hotspots saturate due to locking or busy loops.
- [ ] **Memory**
  - [ ] Set realistic heap and queue capacities:
    - [ ] Confirm memory usage is bounded and proportional to configured queue sizes and number of connections.
    - [ ] Validate behavior when approaching queue capacity (no leaks, predictable message dropping).
- [ ] **File descriptors**
  - [ ] Set system `ulimit -n` according to expected number of connections.
  - [ ] Verify the broker respects limits and logs a clear error when unable to open files or sockets.

---

## 9. Disk-full behavior

- [ ] **WAL on full disk**
  - [ ] Simulate full disk conditions (e.g., via a limited-size filesystem or injected `full_disk_error`):
    - [ ] On WAL append failure:
      - [ ] Broker logs a clear error.
      - [ ] QoS1 publish requests fail gracefully and do not corrupt existing WAL contents.
    - [ ] Verify that the broker does not enter a tight loop on repeated append failures.
- [ ] **Log/metrics when disk is full**
  - [ ] Confirm that errors caused by full disk conditions are surfaced via logs and metrics so operators can react promptly.

---

## 10. WAL corruption behavior

- [ ] **Detection**
  - [ ] Corrupt WAL segments (bit flips) and restart broker:
    - [ ] Ensure CRC mismatches are detected and reported as `Corruption` errors.
- [ ] **Recovery strategy**
  - [ ] Decide and document recovery behavior:
    - [ ] Whether to truncate WAL at the first corrupt record.
    - [ ] Whether to ignore tail segments while preserving earlier valid records.
  - [ ] Confirm implementation matches the documented strategy.
- [ ] **Operator guidance**
  - [ ] Provide clear operational steps for handling detected WAL corruption:
    - [ ] How to back up and truncate the WAL.
    - [ ] How to validate the system after corrective actions.

---

All items above should be tracked in your release checklist, with explicit pass/fail status and links to the test runs, benchmark results, and profiling reports used to validate them.

