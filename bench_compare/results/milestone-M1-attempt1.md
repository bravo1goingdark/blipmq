# Milestone M1 — first measurement (post-Phase-1)

## Setup

- Target: ≥ 5 M msg/s **ingress** (single publisher → single subscriber, QoS0, 64 B).
- Path: real TCP loopback, push delivery (DELIVER frames), batched writer task, TCP_NODELAY on.
- Bench: `cargo bench -p net --bench wire_throughput`. Pre-encodes one PUBLISH payload per case and coalesces 64 PUBLISH frames per `write_all`.
- Hardware: laptop dev box (record `lscpu` for follow-ups).

## Results

| case          | sent      | recv     | drops    | pub_msg/s | deliv_msg/s | MiB/s |
|--------------:|----------:|---------:|---------:|----------:|------------:|------:|
| 100K × 64B    | 100,000   | 82,228   | 17,784   | 1,616,034 | 38,959      | 2.4   |
| 500K × 64B    | 500,000   | 399,605  | 100,423  | 981,852   | 156,671     | 9.6   |
| 1M × 64B      | 1,000,000 | 814,889  | 185,127  | 974,197   | 265,688     | 16.2  |
| 500K × 256B   | 500,000   | 500,000  | 31       | 624,512   | 610,410     | 149.0 |
| 200K × 1KiB   | 200,000   | 200,000  | 185      | 404,491   | 399,222     | 389.9 |

## Reading

- **64-byte case is dominated by per-frame fixed overhead, not bytes.** Drops scale with N because the writer task can't drain at the same rate the publisher fills. The "pub_msg/s" column is misleadingly high — the broker accepts PUBLISH frames quickly, then `try_send` on the per-conn push channel returns `Full` and the broker increments `push_dropped_total`. So the real M1-relevant number is `deliv_msg/s` ≈ **265 K msg/s** at 1M × 64B. That is **~20× short of the 5 M target.**
- **256-byte and 1 KiB cases have no drops.** Throughput plateaus at ~600 K and ~400 K msg/s respectively. Bandwidth is fine (390 MiB/s on a 1 KiB payload), so the limiter is per-frame CPU work in the writer encode path, not network or memcpy.

## Top suspects (in order of expected payoff)

1. **Per-frame allocation in the writer encode path.** `connection::encode_into` calls `DeliverPayload::encode()` which allocates a fresh `BytesMut`, freezes it, then `encode_frame` copies the payload into the writer's batch buffer. Two allocs + two copies per delivered frame. Inlining `DeliverPayload::encode` into the batch buffer would drop both.
2. **`d.topic.as_str().to_string()` per delivery** in `encode_into`. Topic-name clone every message; with `Arc<str>` topics this becomes a refcount bump. (Phase 2 #6 in the plan.)
3. **No publisher backpressure on full push channel.** Drops happen silently. The 64B case hits the limit immediately and 18% of messages are dropped. Phase 6 calls for NACK-when-full; doing it now would also let `pub_msg/s` reflect real ingestion rate.
4. **Single Tokio runtime, default worker count.** Publisher write loop competes with the writer task for the same threads; on a multi-core box increasing worker count alone might help.
5. **`PublishPayload::decode` allocates a `String` for the topic per inbound frame** (`net/src/frame.rs:243`). At 64B this is cheap-but-not-free.

## Lock-in numbers

These two are the M1-tracked metrics going forward:

- `1M × 64B  deliv_msg/s = 265,688` (2026-05-01, post-Phase-1)
- `1M × 64B  drops = 185,127`

Re-run with: `cargo bench -p net --bench wire_throughput`.
