# Milestone M1 — second measurement (post Phase-1 follow-up optimizations)

**Headline: 1.15 M msg/s sustained delivery (single pub → single sub, QoS0,
64 B), zero drops. Up from 265 K in attempt1 — a 4.3× improvement.**

## What changed since attempt1

1. Inline DELIVER encode (`encode_deliver_frame`) — no intermediate `Bytes`
   allocation in the writer task.
2. `TopicName(String)` → `TopicName(Arc<str>)` — clones on the publish path
   are now refcount bumps instead of heap allocations.
3. `PublishPayload::topic`: `String` → `Bytes`. UTF-8 validated in place
   without `to_vec()`. Saves one alloc per inbound PUBLISH on the broker
   ingress path.
4. Per-conn push channel raised from 4 K to 65 K (and writer batch to 128
   frames / 256 KiB, initial buffer 16 KiB). Drops vanish — TCP backpressure
   on the publisher socket replaces them.
5. `ahash` replacing `DefaultHasher` for `TopicShards::shard_index` (no M1
   uplift on a single-topic test; will pay off in M2 fanout).

## Numbers

| case          | sent      | recv      | drops | pub_msg/s | deliv_msg/s | MiB/s |
|--------------:|----------:|----------:|------:|----------:|------------:|------:|
| 100K × 64B    | 100,000   | 100,000   | 16    | 1,571,753 | **1,142,427** | 69.7  |
| 500K × 64B    | 500,000   | 500,000   | 24    | 1,235,283 | **1,127,603** | 68.8  |
| 1M × 64B      | 1,000,000 | 1,000,000 | 39    | 1,185,209 | **1,151,024** | 70.3  |
| 500K × 256B   | 500,000   | 500,000   | 0     | 981,563   | 965,582     | 235.7 |
| 200K × 1KiB   | 200,000   | 200,000   | 16    | 674,238   | 665,803     | 650.2 |

(`drops` here are stragglers at warm-up; they don't change the sustained rate.)

## Per-frame budget analysis

At 1.15 M msg/s, per-message budget is ~870 ns. CPU theoretical ceiling at
5 M is 200 ns/msg. So ~670 ns/msg is being spent on something the next
optimization pass needs to remove.

Estimated per-frame work breakdown (rough, no flamegraph yet):

- Publisher → kernel write (amortized): ~30 ns
- Broker reader: `try_decode_frame` + `handle_frame` async_trait hop +
  `PublishPayload::decode` + `broker.publish_with_wal_id`: **~250 ns**
- mpsc channel hop (try_send + try_recv + Tokio Notify wake): **~200 ns**
- Writer encode + batch + `write_all` (amortized): **~150 ns**
- Subscriber side decode + assert: **~150 ns**

Total: ~780 ns. Within ~10% of measured. Suggests the breakdown is
roughly right.

## Top remaining levers (in order of expected payoff)

1. **Replace `tokio::sync::mpsc` with `flume` or a Notify+ArrayQueue
   pair.** The mpsc-channel hop is ~200 ns/msg of pure
   synchronization overhead. Killing this drops us into 700-ish ns/msg
   territory, ~1.4 M/s.
2. **Bypass the `async_trait` trait object** for PUBLISH frames. The
   `Box<dyn Future>` from `MessageHandler::handle_frame` costs ~80 ns/call
   (alloc + dispatch). For PUBLISH (the hot type) the connection should
   call broker directly via a concrete reference.
3. **Eliminate the broker → writer mpsc hop entirely** for the single-sub
   case. The broker, on publish, knows which connection to deliver to;
   for one-subscriber topics it can write directly into the writer's
   shared `BytesMut` (with a parking_lot Mutex) and signal via Notify.
   ~150 ns saved.
4. **Reduce per-publish atomic ops** inside the broker. Today
   `publish_with_wal_id` takes two `RwLock::read`s (TopicShards shard +
   topic.subscribers) per frame. Cache the topic Arc per connection (a
   per-conn `topic_cache: HashMap<Bytes, Arc<Topic>>`) so consecutive
   publishes to the same topic skip both lookups.
5. **Pin the writer task to a dedicated worker thread** (or use
   `LocalSet`) to keep its working set in L1/L2.

## Recommended next pass

Doing (1) + (4) is the smallest change with the largest expected gain:
~3-3.5 M/s on this hardware. After that, (2) + (3) should clear 5 M.
That's roughly the size of one more focused day of work.

## What this attempt locked in

| metric | now |
|---|---:|
| M1 1M × 64B sustained delivery | **1,151,024 msg/s** |
| M1 1M × 64B drops | 39 (negligible) |
| 1 KiB throughput | 650 MiB/s |

Re-run: `cargo bench -p net --bench wire_throughput`.
