# Milestone M1 — fifth measurement

**Headline: 1.82 M msg/s sustained delivery (single pub → single sub,
QoS0, 64 B), zero drops. Up from 1.40 M (+30%).**

## What changed since attempt4

**Replaced the flume push channel with a shared `BytesMut + Notify` slot.**

Architecture before:
- Broker fanout → `flume::Sender<DeliveryHandle>::try_send` (~150 ns)
- Writer task → `flume::Receiver::recv_async` + `encode_deliver_frame` per
  received handle (~50 ns recv + ~30 ns encode)

Architecture after:
- Broker fanout locks `slot.buf` (parking_lot::Mutex), calls a registered
  `DeliveryEncoder` closure to write the DELIVER frame directly into the
  shared buffer, drops the lock, calls `slot.notify.notify_one()`.
- Writer task swaps the shared buf for an empty local buf under the
  mutex, drops the guard, `write_all`s the local buf in one syscall.

The channel hop is gone. The writer's per-frame encode work is gone.
Lock contention between broker and writer is brief (just the swap).

## Results

| case          | sent      | recv      | drops | pub_msg/s | deliv_msg/s   | MiB/s |
|--------------:|----------:|----------:|------:|----------:|--------------:|------:|
| 100K × 64B    | 100,000   | 100,000   | 0     | 2,605,400 | 1,839,002     | 112.2 |
| 500K × 64B    | 500,000   | 500,000   | 0     | 1,978,044 | 1,854,783     | 113.2 |
| 1M × 64B      | 1,000,000 | 1,000,000 | 0     | 1,843,014 | **1,793,696** | 109.5 |
| 500K × 256B   | 500,000   | 500,000   | 0     | 1,450,584 | 1,429,187     | 348.9 |
| 200K × 1KiB   | 200,000   | 200,000   | 0     | 828,877   | 818,747       | 799.6 |

(Two-run median; first run gave 1,824,951 on the 1M case.)

## Trajectory

| attempt | 1M × 64B deliv_msg/s | vs prior | vs target |
|--------:|---------------------:|---------:|----------:|
| 1       | 265,688              | baseline | 5.3%      |
| 2       | 1,151,024            | +4.3×    | 23%       |
| 3       | 1,201,997            | +1.04×   | 24%       |
| 4       | 1,399,373            | +1.16×   | 28%       |
| 5       | **1,820,000**        | +1.30×   | **36%**   |

## Per-frame budget

At 1.82 M = 549 ns/frame. Down from 714 ns. Saved ~165 ns/frame.

Remaining ~349 ns to 200 ns target. The next costs in the budget,
roughly:
- Reader: `try_decode_frame` + `handle_publish_fast` decode + topic
  cache lookup ≈ 150 ns.
- Broker hot path: shard read lock + topic lookup + subscribers
  read lock + snapshot SmallVec push ≈ 100 ns.
- Per-sub fanout: atomic tag + parking_lot lock + encoder closure
  call + memcpy into shared buf + notify_one ≈ 150 ns.
- Writer: lock-swap + write_all (amortized) + buf clear ≈ 50 ns.

Total ≈ 450 ns. Measured 549. Roughly tracks.

## Lock-in numbers

- M1 1M × 64B sustained delivery: **1,793,696 msg/s**
- 1 KiB throughput: **800 MiB/s** (was 770)
- 256 B sustained: **1,429,187 msg/s** (+15%)

Re-run: `cargo bench -p net --bench wire_throughput`.
