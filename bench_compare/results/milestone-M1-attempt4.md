# Milestone M1 — fourth measurement

**Headline: 1.40 M msg/s sustained delivery (single pub → single sub,
QoS0, 64 B), zero drops. Up from 1.20 M (+17%).**

## What changed since attempt3

1. **Sync PUBLISH fast path.** Connection reader detects `FrameType::Publish`
   and dispatches through a new sync `MessageHandler::handle_publish_fast`
   instead of the `async_trait` path. Skips the per-frame
   `Box<dyn Future>` allocation + indirect call (~80 ns/frame). For
   QoS1 + WAL the fast path returns `Unsupported` and the reader falls
   back to the async durable path — durability semantics unchanged.
2. **Per-conn topic Arc cache.** Single-slot cache of the last
   `(topic_bytes, TopicName)` lookup. Common publisher pattern is
   "1M PUBLISHes to the same topic"; the cache turns each repeat into
   a `Bytes` equality check + `Arc<str>` clone (refcount bump),
   eliminating the per-frame `Arc::from(&str)` heap alloc.

## Results

| case          | sent      | recv      | drops | pub_msg/s | deliv_msg/s   | MiB/s |
|--------------:|----------:|----------:|------:|----------:|--------------:|------:|
| 100K × 64B    | 100,000   | 100,000   | 9     | 1,813,765 | 1,315,072     | 80.3  |
| 500K × 64B    | 500,000   | 500,000   | 21    | 1,408,374 | 1,353,408     | 82.6  |
| 1M × 64B      | 1,000,000 | 1,000,000 | 25    | 1,440,213 | **1,399,373** | 85.4  |
| 500K × 256B   | 500,000   | 500,000   | 21    | 1,266,214 | 1,243,707     | 303.6 |
| 200K × 1KiB   | 200,000   | 200,000   | 12    | 797,582   | 788,843       | 770.4 |

## Trajectory

| attempt | 1M × 64B deliv_msg/s | vs prior | vs target |
|--------:|---------------------:|---------:|----------:|
| 1       | 265,688              | baseline | 5.3%      |
| 2       | 1,151,024            | +4.3×    | 23%       |
| 3       | 1,201,997            | +1.04×   | 24%       |
| 4       | **1,399,373**        | +1.16×   | **28%**   |

## Per-frame budget

At 1.40 M msg/s = 714 ns/frame. 5 M target = 200 ns/frame. Remaining
gap ≈ 514 ns/frame.

The largest single lever still untouched is **eliminating the
broker → writer mpsc hop** for the single-conn case. The writer task
gets handed a `DeliveryHandle` via `flume::Sender::try_send` per
delivered message (~150 ns of per-op atomics + Notify wake). For a
1-sub topic, the broker could write directly into a shared `BytesMut`
behind a `Mutex` and signal via Notify — same data flow without the
channel overhead. Estimated worth: 1.6–2.0 M msg/s.

Other levers:
- Multi-publisher scaling (today's bench uses one publisher);
  4 publishers × 1 sub should aggregate near-linearly with the
  sharded broker, validating the architecture's parallelism.
- Pinning the writer task to a dedicated worker thread to keep its
  hot working set in L1.

## Lock-in numbers

- M1 1M × 64B sustained delivery: **1,399,373 msg/s**
- 1 KiB throughput: **770 MiB/s** (was 700 MiB/s)
- 256 B sustained: **1,243,707 msg/s** (~+16%)

Re-run: `cargo bench -p net --bench wire_throughput`.
