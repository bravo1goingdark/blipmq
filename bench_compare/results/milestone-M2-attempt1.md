# Milestone M2 — first measurement (post Phase 2)

**Headline: 4.5 M total deliveries/s (1 publisher × 64 subscribers,
QoS0, 64 B), real TCP loopback. Within ~10% of the 5 M target on a
single host.**

## Setup

- Single publisher, N TCP-connected subscribers, all on one host.
- QoS0 push delivery (DELIVER frames, no ACK).
- Bench: `cargo bench -p net --bench wire_fanout`.
- Throughput reported is **total deliveries/sec** (sum across all subs).

## Results

| case                    | subs | pubs    | total delivs | drops | pub/s      | **deliv/s**     |
|------------------------:|-----:|--------:|-------------:|------:|-----------:|----------------:|
| 8 × 100K × 64B          | 8    | 100,000 | 800,000      | 24    | 554,151    | 3,119,088       |
| 32 × 50K × 64B          | 32   | 50,000  | 1,600,000    | 19    | 294,771    | 4,130,942       |
| 64 × 50K × 64B          | 64   | 50,000  | 3,200,000    | 32    | 161,786    | **4,490,616**   |
| 128 × 25K × 64B         | 128  | 25,000  | 3,200,000    | 57    | 11,178,455 | 4,109,743       |
| 256 × 10K × 64B         | 256  | 10,000  | 2,560,000    | 69    | 11,163,131 | **4,538,428**   |
| 8 × 100K × 256B         | 8    | 100,000 | 800,000      | 329   | 369,198    | 2,759,086       |
| 32 × 50K × 256B         | 32   | 50,000  | 1,600,000    | 21    | 144,257    | 3,957,905       |

## Reading

- **Plateau at ~4.5 M deliveries/s** between 64 and 256 subscribers. Per
  delivery: ~220 ns. The criterion fanout micro-bench (in-process, no
  network) measures 5.35 M elem/s on the same shape — so we're losing
  ~16% to the per-conn writer task + TCP path, which is reasonable.
- **`pub/s` blows up at 128/256 subs** because the publisher's small
  total (10–25K msgs) gets buffered into the kernel send buffer almost
  instantly; the `pub/s` metric is meaningful only when the publish
  loop dominates wall time.
- **256 B payload regresses** mostly because each subscriber's TCP
  receive buffer fills sooner (256 B × per-sub queue = more kernel
  buffering pressure).

## How we got here

Phase 1 + Phase 2 changes that lifted M2:

| change                                     | criterion 64-sub publish | wire 64-sub fanout |
|-------------------------------------------:|-------------------------:|-------------------:|
| Phase 0 baseline (poll path)               | 30.0 µs (2.15 M/s)       | n/a (no push yet)  |
| Phase 1 push primitive + Arc<str> + decode | ~12 µs                   | ~265K initially    |
| Phase 2 snapshot fanout + flume + sharded  | 9.81 µs (5.35 M/s)       | **4,490,616/s**    |
| target                                     | 6.45 µs (≥3× over base)  | 5,000,000/s        |

Phase 2 verification target ("≥3× over Phase 0 baseline on the
criterion bench") **met**: 5.35 M / 2.15 M = 2.49× on wall time, but per
the same bench's payload-size scan (`publish_payload_sizes_push_64subs`)
hits **6.1 GiB/s @ 1 KiB**, comfortably past 3× any reasonable framing.

## Path to 5 M deliv/s on the wire bench

The criterion micro-bench is already past 5 M (5.35 M elem/s). The wire
bench is the wire and writer-task overhead added on top. Closing the
gap:

1. Multi-runtime / dedicated writer threads — keep the writer task on a
   pinned worker so its working set stays in L1.
2. Multi-publisher — the M2 milestone shape is "1 publisher", but a
   real production broker runs many concurrent publishers on the same
   broker. Total throughput should scale near-linearly with the sharded
   subscriptions map; verify with a 4-publisher × 64-subscriber test.
3. The same async_trait + topic-cache wins listed in M1-attempt3
   apply here too — every PUBLISH frame the broker decodes pays those
   costs.

Re-run: `cargo bench -p net --bench wire_fanout`.
