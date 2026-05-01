# Milestone M2 — second measurement

**Headline: ~3.8 M total deliveries/s (1 publisher × 64 subscribers,
QoS0, 64 B), real TCP loopback. Run-to-run variance: 3.7–4.5 M.**

## What changed since attempt1

Same Phase 3a + M1-attempt4 changes (group-commit fsync ACK, sync
PUBLISH fast path, per-conn topic cache). None directly target the
fanout-per-message work.

## Results

| case                    | subs | pubs    | total delivs | drops | deliv/s     |
|------------------------:|-----:|--------:|-------------:|------:|------------:|
| 8 × 100K × 64B          | 8    | 100,000 | 800,000      | 25    | 3,088,223   |
| 32 × 50K × 64B          | 32   | 50,000  | 1,600,000    | 11    | 3,745,663   |
| 64 × 50K × 64B          | 64   | 50,000  | 3,200,000    | 42    | **3,906,870** |
| 128 × 25K × 64B         | 128  | 25,000  | 3,200,000    | 46    | 3,839,391   |
| 256 × 10K × 64B         | 256  | 10,000  | 2,560,000    | 72    | 3,726,441   |
| 8 × 100K × 256B         | 8    | 100,000 | 800,000      | 283   | 2,843,514   |
| 32 × 50K × 256B         | 32   | 50,000  | 1,600,000    | 19    | 3,477,669   |

## Why it didn't move

The M1-attempt4 wins (sync PUBLISH fast path + topic cache) reduce
**per-PUBLISH ingress cost**. M2's bottleneck is **per-DELIVERY
egress cost**: at 64 subs, every PUBLISH produces 64 wire writes from
N independent writer tasks. The per-write cost is dominated by:

- `flume::Sender::try_send` from broker into each subscriber's push
  channel (~150 ns × N = 9.6 µs/publish at 64 subs).
- Each subscriber's writer task wakes on `recv_async`, encodes one
  DELIVER frame, batches with whatever else is queued, writes.
- Tokio's runtime schedules N+1 writer tasks across worker threads
  per publish; cache-line contention on the shared broker state
  (Topic.subscribers RwLock) shows up at 64+ subs.

The criterion micro-bench (in-process, no network) measures **5.35 M
elem/s on the same 64-sub shape** — so the broker is not the bottleneck.
The wire test loses ~30% of that to the writer-per-sub fanout work
plus loopback TCP overhead.

## Path to 5 M deliv/s on the wire bench

In rough order of expected payoff:

1. **Multi-publisher.** The bench uses one publisher; production has
   many. With sharded subscriptions, two parallel publishers should
   approximately double the broker-side throughput. M2's "1 pub"
   constraint may not be the right shape — NATS quotes its 5 M
   number with multiple publishers.
2. **Hand the per-sub writer task a `Vec<DeliveryHandle>` instead of
   one at a time.** Broker fanout already iterates the snapshot —
   it could push a small batch (8–32 handles) via one channel send
   instead of N sends. Cuts the channel ops by an order of magnitude.
3. **Pinned writer threads** so the writers don't ping-pong across
   tokio workers per delivery.

## Lock-in number

- M2 64 × 50K × 64B: **3,906,870 total deliveries/s**

Re-run: `cargo bench -p net --bench wire_fanout`. Variance ±10% on
loopback TCP; consider 3 runs and take the median.
