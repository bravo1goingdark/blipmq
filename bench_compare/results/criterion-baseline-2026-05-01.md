# Criterion fanout baseline — 2026-05-01

Captured from `cargo bench -p core --bench fanout -- --quick` on `main`
@ commit `1d563c3` (pre-Phase-1 changes).

Hardware: see `uname -a` / `lscpu` on the build host.

## publish_qos0_fanout (Bytes payload = 256 B)

| subscribers | mean time | throughput (elem/s) |
|------------:|----------:|--------------------:|
| 1           | 454 ns    | 2.20 M              |
| 8           | 3.23 µs   | 2.48 M              |
| 64          | 29.8 µs   | 2.15 M              |
| 512         | 181 µs    | 2.83 M              |

Scaling is roughly linear in subscriber count: ~350–460 ns per subscriber
per published message. This time is dominated by `parking_lot::Mutex`
acquire/release on `SubscriberQueueInner` plus the `HashMap` /
`VecDeque` / `BinaryHeap` mutations inside `SubscriberQueue::enqueue`
(`core/src/lib.rs:331`).

## publish_qos1_fanout (Bytes payload = 256 B)

| subscribers | mean time | throughput (elem/s) |
|------------:|----------:|--------------------:|
| 1           | 384 ns    | 2.60 M              |
| 8           | 3.20 µs   | 2.50 M              |
| 64          | 21.9 µs   | 2.92 M              |
| 512         | 182 µs    | 2.82 M              |

Indistinguishable from QoS0 — confirms the QoS distinction lives at
`dequeue`, not `enqueue`. Phase 1 publish-path improvements apply
equally.

## publish_payload_sizes_qos0_64subs (64 subscribers)

| payload | mean time | bytes/s    |
|--------:|----------:|-----------:|
| 64 B    | 31.5 µs   | 124 MiB/s  |
| 1 KiB   | 30.2 µs   | 2.02 GiB/s |
| 16 KiB  | 30.5 µs   | 30.7 GiB/s |

Time is **flat across payload size** because `Bytes::clone()` is an Arc
refcount bump, not a memcpy. Validates the existing zero-copy fanout
strategy and means Phase 1's encode-buffer pooling will only help on
the wire-encode side (one copy per subscriber on write), not on the
broker fanout itself.

## What this baseline locks in

- **Phase 1 target:** publish-path throughput ≥ 10 M elem/s for the
  pure-enqueue micro-bench at 64 subs (currently 2.1 M). This requires
  killing the per-subscriber mutex on the fanout path — the explicit
  goal of Phase 1 #2 ("the channel *is* the pending queue") and Phase 2
  #3 ("drop the publish-path mutex").
- **Phase 2 target:** ≥ 3× over Phase 1 on the same micro-bench (sharded
  subscriptions + ahash + topic-name interning).
- The payload-size bench should remain flat after Phase 1 (regression
  guard against accidentally introducing a memcpy on fanout).

Re-run command: `cargo bench -p core --bench fanout` (drop `--quick`
for a full sample-100 run).
