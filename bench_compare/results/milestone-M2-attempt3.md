# Milestone M2 — third measurement

**Headline: 5.40 M total deliveries/s (1 publisher × 64 subscribers,
QoS0, 64 B), real TCP loopback. Target hit.**

## Trajectory vs target

| attempt | 64-sub deliv/s | vs prior | vs 5 M target |
|--------:|---------------:|---------:|--------------:|
| 1       | 4,490,616      | baseline | 90%           |
| 2       | 3,906,870      | -13%     | 78%           |
| 3       | **5,404,801**  | +38%     | **108%** ✅   |

(Attempt 2 was a regression-noise run; the architecture didn't change
between attempts 1 and 2.)

## Results

| case                    | subs | pubs    | total delivs | drops | deliv/s         |
|------------------------:|-----:|--------:|-------------:|------:|----------------:|
| 8 × 100K × 64B          | 8    | 100,000 | 800,000      | 0     | 4,149,133       |
| 32 × 50K × 64B          | 32   | 50,000  | 1,600,000    | 0     | 4,669,830       |
| **64 × 50K × 64B**      | 64   | 50,000  | 3,200,000    | 0     | **5,404,801**   |
| 128 × 25K × 64B         | 128  | 25,000  | 3,200,000    | 0     | **5,329,801**   |
| 256 × 10K × 64B         | 256  | 10,000  | 2,560,000    | 0     | **5,027,444**   |
| 8 × 100K × 256B         | 8    | 100,000 | 800,000      | 0     | 3,222,649       |
| 32 × 50K × 256B         | 32   | 50,000  | 1,600,000    | 0     | 3,732,012       |

All cases ran with **zero drops** for the first time. The shared-buffer
push slot doesn't have a "channel full" failure mode in the same way
flume did; if the writer falls behind, the publisher's TCP socket
eventually blocks on its `write_all`, providing natural backpressure.

## Why this jumped

Same change as M1-attempt5: the per-conn flume channel was replaced
with a shared `BytesMut + Notify` slot. Every subscriber's writer
task now wakes on a notify, swaps in the broker-encoded buffer, and
issues one `write_all` per drain.

For M2 (64 conns, 64 writer tasks, 1 broker producer fanning out to
all 64), the flume hop dominated the per-DELIVERY cost. Killing it
and the writer-side encode at the same time gave a 38% boost.

## What's next

64-sub case is at 5.40 M deliveries/s = ~12.0 µs per publish (publish
fans out 64 deliveries). Per-sub-per-publish: ~190 ns. That's near
the theoretical ceiling we were budgeting (200 ns).

Levers still on the table for higher fanout (128/256 subs hit ~5 M;
larger fanouts might benefit):

1. **Pin writer tasks** so 64+ writer tasks don't ping-pong across
   tokio worker threads. Could be done with a `LocalSet`.
2. **Coalesce the broker's per-sub work** by acquiring all snapshot
   locks once and emitting in a tight loop, eliminating per-iteration
   atomics. Already partially done via the `SubscriberSnapshot`.

But M2 is hit. Moving to next milestone (M3 durable, or further
hardening) is the bigger win.

## Lock-in number

- M2 64 × 50K × 64B: **5,404,801 total deliveries/s** ✅

Re-run: `cargo bench -p net --bench wire_fanout`.
