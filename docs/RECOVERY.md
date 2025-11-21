# BlipMQ Recovery & Failure Handling

BlipMQ uses a write-ahead log (WAL) to provide durable QoS1 semantics and crash recovery. This document describes the WAL format, replay rules, and recommended recovery drills.

## WAL Format

The WAL implemented by `blipmq_wal` has a simple binary layout:

```text
[32-byte header]
  magic   = "BLIPWAL\0" (8 bytes)
  version = 1           (4 bytes)
  ... padding/reserved  (remaining bytes)

[record 1][record 2][record 3]...

record:
  [u64 id][u32 len][u32 crc32][len bytes payload]
```

- `id`:
  - Monotonically increasing logical record id.
- `len`:
  - Payload length in bytes.
- `crc32`:
  - CRC-32 of the payload.

On open:

1. If the file is empty:
   - Header is written and `next_id` starts at 1.
2. If the file has content:
   - Header is validated (magic & version).
   - Records are scanned sequentially:
     - CRC is verified for each record.
     - `index: HashMap<u64, u64>` is built mapping `id -> offset`.
   - `next_id` is set to `last_id + 1`.

If a CRC mismatch or structural error is detected during scanning, `WriteAheadLog::open` returns `WalError::Corruption`.

## Broker WAL Encoding

The broker (`blipmq_core`) encodes messages into WAL records via `WalMessageRecord`:

```rust
struct WalMessageRecord {
    topic: String,
    qos: QoSLevel,
    payload: Bytes,
}
```

Encoded payload:

```text
[u8 qos][u16 topic_len][topic_bytes...][payload_bytes...]
```

- `qos`:
  - `0` (QoS0) or `1` (QoS1).
- `topic_len`:
  - Length of topic name.
- `topic_bytes`:
  - UTF-8 topic.
- `payload_bytes`:
  - Opaque message payload.

Decode failures (invalid QoS, truncated topic, invalid UTF-8) result in `LogError::Corruption`.

## Replay & Recovery Rules

When a broker is constructed with `Broker::new_with_wal`:

1. WAL is opened with `WriteAheadLog::open_with_config`.
2. At startup, `Broker::replay_from_wal().await` is invoked:
   - Calls `wal.iterate_from(start_id)` (usually the first id).
   - For each `WalRecord`:
     - Decodes `WalMessageRecord`.
     - Reconstructs `TopicName` and `QoSLevel`.
     - Publishes messages back into subscriber queues.

Semantics:

- QoS1:
  - Messages that were not acknowledged are re-enqueued.
  - Messages that were acknowledged may also be re-enqueued:
    - ACK state is in-memory only.
    - This yields **at-least-once** delivery across crashes.
- QoS0:
  - If stored in WAL, may also be replayed; QoS0 clients should treat them as best-effort.

## Crash Scenarios

### Crash Before WAL Append

If a crash occurs before `publish_durable` can append to WAL:

- The message is not present in WAL.
- It will not be replayed or redelivered after restart.
- The client may observe:
  - A NACK.
  - A connection error.

At-least-once semantics are preserved for successfully appended records.

### Crash After WAL Append, Before Enqueue

If crash occurs after WAL append but before enqueueing into subscriber queues:

- The record is present in WAL.
- On replay, message is enqueued and delivered.
- Client may see delayed or duplicate delivery but not loss.

### Crash After Delivery, Before ACK

For QoS1:

- Message was appended to WAL and delivered, but ACK not durably tracked.
- On restart:
  - WAL is replayed.
  - Message is re-enqueued.
  - Client receives the message again.

Clients should be prepared to handle duplicates (at-least-once semantics).

### Crashed Process with Inflight Messages

If the broker crashes while messages are in inflight queues:

- Inflight state lives only in memory.
- On restart:
  - Replay will reintroduce messages that were recorded in WAL.
  - Any messages that were only in memory and not yet appended are lost.

QoS1 guarantees that messages visible to the subscriber will have been appended prior to delivery when using `publish_durable`.

## Corruption Handling

If WAL has been corrupted (e.g. due to disk failure or manual modification):

- `WriteAheadLog::open()` returns `WalError::Corruption`.
- `blipmqd` startup using that WAL path will fail.
- No partial replay of corrupted content occurs.

Operationally:

- Treat corruption as a critical incident.
- Restore from a known-good backup or snapshot where possible.
- Investigate underlying storage issues.

## Recovery Drills

To validate recovery behavior in non-production environments, run the following drills.

### Drill 1: QoS1 Crash Recovery

Goal: Ensure unacknowledged QoS1 messages are redelivered after a crash.

Steps:

1. Start `blipmqd` with WAL enabled (`fsync_policy` set to something reasonable).
2. Create a durable QoS1 subscription to a topic, e.g., `"chaos-topic"`.
3. Publish N QoS1 messages using a client.
4. ACK approximately half of them from the subscriber.
5. Simulate crash:
   - In tests: use `blipmq_chaos::simulate_crash(broker)` or simply drop the broker.
   - In a running daemon: kill the process (in a controlled environment).
6. Restart `blipmqd` with the same WAL path.
7. Allow the broker to call `replay_from_wal`.
8. Resubscribe and poll messages.

Expected behavior:

- All originally published messages are observed at least once.
- All previously unacknowledged messages are definitely redelivered.
- ACKed messages may also be redelivered.

This behavior is tested in `tests/chaos_recovery.rs`.

### Drill 2: WAL Corruption at Tail

Goal: Confirm that corruption is detected and fails startup clearly.

Steps:

1. Run a program that uses `WriteAheadLog` to append a few records.
2. Close the program to ensure WAL is flushed.
3. Use `blipmq_chaos::corrupt_file_tail_bit(path)` to flip a bit near the end of the WAL.
4. Attempt to open the WAL again via `WriteAheadLog::open(path)`.

Expected behavior:

- `WalError::Corruption` is returned.
- No partial or silent truncation is accepted.

Operational readiness:

- Document procedures for:
  - Restoring WAL from backup.
  - Inspecting logs for specific corruption messages.

### Drill 3: Disk-Full Simulation

Goal: Validate behavior when WAL cannot append due to disk-full.

Steps:

1. Use `blipmq_chaos::full_disk_error()` in a test harness to simulate IO errors during WAL append.
2. Ensure `publish_durable` translates this into:
   - A NACK to the client.
   - Appropriate logging.
3. Confirm:
   - Non-durable paths may continue if you choose to allow them.
   - System metrics and logs highlight the disk pressure.

Operational guidelines:

- Monitor disk usage and set alerts well before full capacity.
- Ensure disk-full conditions are treated as high-severity incidents.

### Drill 4: Slow Disk Simulation

Goal: Evaluate how BlipMQ behaves with high WAL latency.

Steps:

1. Wrap WAL writes in tests with `blipmq_chaos::simulate_slow_disk_write`:

   ```rust
   let result = simulate_slow_disk_write(Duration::from_millis(50), || async {
       wal.append(b"payload").await
   }).await;
   ```

2. Run benchmark harness or integration tests under this slower WAL.

Expected observations:

- Increased tail latency for QoS1 with WAL.
- Lower throughput compared to normal environment.
- No data corruption or logical inconsistencies.

### Drill 5: Network Disconnects

Goal: Ensure clients and broker behave correctly under intermittent network failures.

Steps:

1. Use `blipmq_chaos::maybe_disconnect(&mut TcpStream, probability)` in clients.
2. Run end-to-end tests:
   - Frequent random disconnects at various points (before/after AUTH, during publish, during poll).
3. Validate:
   - Clients can reconnect and re-authenticate.
   - Subscriptions can be re-established.
   - QoS1 messages are either redelivered or acknowledged as expected.

## Example: Corrupt WAL Segment & Expected Behavior

The following example describes a typical corruption scenario and expected response.

1. Start `blipmqd` and publish a batch of QoS1 messages.
2. Stop `blipmqd` cleanly to ensure WAL is flushed.
3. Introduce corruption:

   ```rust
   use blipmq_chaos::corrupt_file_tail_bit;

   corrupt_file_tail_bit("/var/lib/blipmq/blipmq.wal")
       .expect("failed to corrupt WAL");
   ```

4. Start `blipmqd` again.

Expected behavior:

- `WriteAheadLog::open` detects CRC mismatch and returns `WalError::Corruption`.
- `blipmqd` logs a clear error and aborts startup.
- No partial replay from the corrupted WAL occurs.

Operator actions:

- Investigate underlying storage or filesystem issues.
- Restore WAL from a known-good snapshot or backup.
- Restart BlipMQ with a clean or repaired WAL once root cause is addressed.

By regularly executing these drills and verifying behavior, teams can ensure BlipMQ behaves predictably under failure and that operational runbooks remain accurate and effective.
