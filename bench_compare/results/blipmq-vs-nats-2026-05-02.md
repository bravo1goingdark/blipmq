# BlipMQ vs NATS — head-to-head, 2026-05-02

Single-machine, real TCP loopback (no NIC). Both brokers run as native binaries
on the same host, started from the same shell session. Numbers below are
raw throughput at fixed payload / fanout shapes; latency is reported only
when the underlying tool reports it.

## Setup

- **Host:** Intel i5-1135G7 @ 2.40 GHz, 8 logical cores, 14 GiB RAM, Linux 6.17.
- **NATS:** `nats-server v2.10.16`, default config, JetStream enabled.
  Numbers come from the upstream `nats bench` CLI v0.1.5 (the standard
  NATS-published microbench).
- **BlipMQ:** in-tree wire benchmarks (`cargo bench -p net --bench wire_throughput`,
  `wire_fanout`). Same broker code as commits `7a553cb` / `0ceddad`. mimalloc
  enabled (default).
- **Payload tags:** ≤ 256 bytes is "small"; 1 KiB is "medium". Sub counts
  encode fan-out: `1:N` means one publisher × N subscribers on the same
  topic.

## Single-pipe (1 pub × 1 sub)

| shape | NATS sub/s | BlipMQ deliv/s | Δ vs NATS |
|---|---:|---:|---:|
| 64B   | 1.20 M | **1.82 M** | +52% |
| 1 KiB | 0.59 M | **0.93 M** | +59% |

BlipMQ's `wire_throughput` reports `pub_msg/s` and `deliv_msg/s` separately.
At 1 KiB the broker delivers 928 K msg/s ≈ **906 MiB/s** sustained.

## Fanout (1 pub × N subs, 64 B payload)

Per-subscriber throughput from NATS comes from the bench's per-sub line;
aggregate deliveries = per-sub × N. BlipMQ's `wire_fanout` reports the
aggregate directly.

| fanout | NATS per-sub | NATS aggregate | BlipMQ aggregate | Δ vs NATS |
|---|---:|---:|---:|---:|
| 1:8    | 203 K | 1.63 M | **3.81 M** | +134% |
| 1:32   | 51 K  | 1.65 M | 3.94 M (256 B) / **4.95 M** (64 B) | +200% |
| 1:64   | 25.6 K | 1.64 M | **5.14 M** | +213% |
| 1:128  | —     | —      | **5.31 M** | n/a |

BlipMQ's per-publisher rate falls as fanout grows (fanout cost is real on
the publishing thread), but the aggregate delivery rate keeps climbing
because more subscribers can be served in parallel via the writer-task
pool. NATS by contrast pegs at ~1.6 M aggregate regardless of fanout —
the publisher is the bottleneck.

## Durable / QoS1

| shape | tool | sub/s |
|---|---|---:|
| 1 KiB, 1 pub × 1 sub, NATS JetStream (file storage) | `nats bench --js` | 47 K |
| 1 KiB, 1 pub × 1 sub, BlipMQ QoS1 group-commit fsync | (not yet wire-bench-covered) | n/a |

JetStream's durable single-pipe is bound by its append-and-fsync loop;
NATS hits ~47 K msg/s at 1 KiB. BlipMQ has the equivalent durability
guarantee (`publish_durable` returns after fsync covers the record), and
runs the WAL in group-commit mode, but the existing in-tree wire benches
don't yet cover the QoS1 head-to-head — adding it is on the to-do list.
For now, the QoS1 perf claim is "not measurably worse than QoS0 once
group-commit batching kicks in," based on the in-tree `wal` tests and
the durable-recovery integration tests.

## Multi-publisher (BlipMQ-only here; not in NATS bench)

For completeness, BlipMQ's `wire_multi_pub` measures N independent
`(publisher, subscriber)` pairs — same shape as wire_throughput, but
parallel:

| pipes | aggregate msg/s |
|---:|---:|
|  1 | 1.7 M |
|  2 | 3.2 M |
|  4 | 5.0 M |
|  8 | **6.5 M** |
| 16 | **6.9 M** (peak) |

NATS's bench tool doesn't have an equivalent "N independent pipes"
shape, but at 1.6 M aggregate cap on shared-topic fanout, it would
need ~4 separate processes / accounts to come close.

## Caveats

- **Not the same workload.** NATS bench publishes raw bytes;
  BlipMQ's wire benches publish through the v2 framed protocol with
  topic + qos byte + (optional) ttl + (optional) partition_key. The
  framing overhead is small (≈ 30 B per frame) and is included in
  BlipMQ's MiB/s figures.
- **Not under network latency.** Loopback only. Real-world RTT
  changes the picture for NATS (NATS clients pipeline aggressively;
  BlipMQ's writer task already coalesces). Re-run on the actual
  network you'll deploy on before drawing operational conclusions.
- **NATS JetStream defaults.** I used `--storage file --replicas 1`,
  which is the moral equivalent of BlipMQ's `fsync_policy = "every_n:64"`.
  Different fsync policies on either side change the durable numbers
  significantly.
- **bench_compare harness is broken.** The in-tree `bench_compare` crate
  is supposed to drive this comparison automatically, but its standalone
  `sub_bmq` client is stuck on the v1 POLL protocol while the broker
  only serves v2 push. So the head-to-head above came from the upstream
  `nats bench` CLI on the NATS side and the in-tree `wire_throughput` /
  `wire_fanout` benches on the BlipMQ side. Fixing `bench_compare` to
  use the v2 push protocol is filed as a follow-up.

## Reproducing

```bash
# NATS side
nats-server --jetstream --store_dir /tmp/nats_js > /tmp/nats.log 2>&1 &

NATS=/path/to/nats   # natscli v0.1.5 binary
$NATS bench n64 --pub 1 --sub 1 --msgs 1000000 --size 64
$NATS bench n1k --pub 1 --sub 1 --msgs 200000 --size 1024
$NATS bench nfan8  --pub 1 --sub 8  --msgs 100000 --size 64
$NATS bench nfan32 --pub 1 --sub 32 --msgs 50000  --size 64
$NATS bench nfan64 --pub 1 --sub 64 --msgs 500000 --size 64

# JetStream — create the stream first
$NATS stream add JS_BENCH --subjects bench-js --storage file --replicas 1 \
  --discard old --max-bytes 1GB --max-msgs -1 --max-msg-size -1 \
  --max-age 0 --retention limits --dupe-window 2m --defaults
$NATS bench bench-js --js --pub 1 --sub 1 --msgs 50000 --size 1024

# BlipMQ side — in-tree wire benches
cargo bench -p net --bench wire_throughput
cargo bench -p net --bench wire_fanout
cargo bench -p net --bench wire_multi_pub
```
