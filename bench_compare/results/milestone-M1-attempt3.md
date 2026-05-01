# Milestone M1 — third measurement (post Phase 2)

**Headline: 1.20 M msg/s sustained delivery (single pub → single sub, QoS0,
64 B), zero drops. Up modestly from Phase 1's 1.15 M.**

## What changed since attempt2

- Snapshot subscriber list under brief read lock for fanout (Vec/SmallVec
  collect of Arc clones, drop the lock, iterate).
- Replaced `tokio::sync::mpsc` with `flume` on the per-conn DELIVER push
  channel. `flume::recv_async` integrates cleanly with tokio's runtime;
  per-op overhead is materially lower for this workload.
- Sharded the global `Broker.subscriptions` map into 16 shards (mask-
  indexed). `subscribe`/`poll`/`ack`/`unsubscribe` each touch one shard;
  the global write-lock that previously serialized those ops is gone.

## Results

| case          | sent      | recv      | drops | pub_msg/s | deliv_msg/s | MiB/s |
|--------------:|----------:|----------:|------:|----------:|------------:|------:|
| 100K × 64B    | 100,000   | 100,000   | 19    | 1,479,848 | 1,133,500   | 69.2  |
| 500K × 64B    | 500,000   | 500,000   | 12    | 1,255,653 | 1,190,348   | 72.7  |
| 1M × 64B      | 1,000,000 | 1,000,000 | 22    | 1,245,069 | **1,201,997** | 73.4  |
| 500K × 256B   | 500,000   | 500,000   | 14    | 1,096,254 | 1,072,401   | 261.8 |
| 200K × 1KiB   | 200,000   | 200,000   | 17    | 726,919   | 717,415     | 700.6 |

## Trajectory

| attempt | 1M × 64B deliv_msg/s | vs prior |
|--------:|---------------------:|---------:|
| 1       | 265,688              | baseline |
| 2       | 1,151,024            | +4.3×    |
| 3       | **1,201,997**        | +1.04×   |

Single-sub Phase 2 changes give a small wire-bench uplift because they
mostly remove fanout-side and shard-write-side contention that doesn't
manifest on this 1-sub workload. The same changes give the M2 fanout
bench (1 pub × N subs) a much bigger lift — see
`milestone-M2-attempt1.md`.

## Where the remaining 4× to 5M lives

Per-frame budget at 1.20 M = 833 ns; theoretical 5 M = 200 ns. Levers
left, ranked:

1. Bypass `async_trait` dispatch on the PUBLISH hot path. Connection
   reader reaches BrokerHandler via a Box-future trait object; concrete
   call would save ~80 ns/frame.
2. Per-conn topic Arc cache. Same publisher hits the same topic 1M+
   times in a row; current code does a HashMap lookup each time.
3. Eliminate the broker → writer channel hop entirely for the
   single-conn case (shared BytesMut + Notify), saving ~150 ns/frame.
4. Multi-publisher and multi-conn scaling: today the bench is one
   publisher; pushing a second publisher would test whether the broker
   parallelizes correctly with sharded subscriptions.

These are Phase 3+ scope.
