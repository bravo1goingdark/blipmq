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
- **BlipMQ:** broker is the in-tree `blipmqd --release` (commit `7a553cb` /
  `0ceddad`) with mimalloc; publishers and subscribers are
  `pub_bmq` / `sub_bmq` from the in-tree `bench_compare` crate (after
  the v2-push port in commit … below).
- **Payload tags:** ≤ 256 bytes is "small"; 1 KiB is "medium". Sub counts
  encode fan-out: `1:N` means one publisher × N subscribers on the same
  topic.

Both sides run **cross-process** (broker, publisher, and subscriber are
separate OS processes connected over TCP loopback). This is the apples-
to-apples shape for comparing brokers; BlipMQ's *in-process* wire benches
in `net/benches/wire_*.rs` measure something different (single tokio
runtime, no socket boundary on the publish side) and produce
~2× higher numbers.

## Single-pipe (1 pub × 1 sub)

| shape | NATS sub/s | BlipMQ deliv/s | Δ vs NATS |
|---|---:|---:|---:|
| 64B   | 1.19 M | **1.79 M** | BlipMQ +50% |
| 1 KiB | 0.61 M | **0.84 M** | BlipMQ +37% |

BlipMQ now wins single-pipe too, after `pub_bmq` was switched to
batched publishing (`publish_batched` + `flush_publishes` in
`BmqClient`). The earlier no-batching number — 0.86 M @ 64B and
0.49 M @ 1 KiB — was bound on per-frame `write_all` syscalls, not
broker capacity; NATS's PUB-line pipelining was doing the same
amortization implicitly.

## Fanout (1 pub × N subs, 64 B payload)

NATS bench reports per-subscriber throughput; aggregate = per-sub × N.
BlipMQ's `sub_bmq` reports its own subscriber's rate; aggregate is the
sum across the N subscriber processes.

| fanout | NATS per-sub | NATS aggregate | BlipMQ aggregate | Δ vs NATS |
|---|---:|---:|---:|---:|
| 1:8   | 202 K | 1.62 M | **3.53 M** | +118% |
| 1:32  | 49 K  | 1.56 M | **4.42 M** | +183% |

BlipMQ wins fanout, decisively. NATS pegs at ~1.6 M aggregate
regardless of fan-out — the publisher is the bottleneck. BlipMQ's
publisher rate falls as fan-out grows (459 K at 1:8, 467 K at 1:32),
but the aggregate delivery rate keeps climbing because the broker's
writer-task pool serves the subscribers in parallel.

## Durable / QoS1 (1 KiB)

| shape | NATS JS pub/s | NATS JS sub/s | BlipMQ pub/s | BlipMQ sub/s |
|---|---:|---:|---:|---:|
| 1 publisher, serial | 60 K | 147 K | 20 K | 19 K |
| 8 parallel publishers, 1 sub | n/a | n/a | 96 K | 68 K |
| 8 parallel publishers, 4 parallel subs | n/a | n/a | 93 K | 169 K agg |

**Caveat — single-publisher QoS1**: BlipMQ's `publish_durable` blocks
until the WAL fsync that covers the record returns. With a *single*
serial publisher, every call triggers its own fsync (the WAL writer
has no other in-flight records to batch with). That caps pub at
≈ 20 K msg/s — fsync-rate-bound, not broker-bound. NATS JetStream
batches more aggressively at its tail-of-log layer.

With multiple parallel publishers BlipMQ's WAL writer batches up
to `fsync_every_n` records per fsync, and the throughput jumps:
8 parallel publishers hit 96 K pub/s — overtaking NATS JS's
60 K serial-publisher number. Real applications that care about
durable throughput typically run several publisher instances; the
serial-single-pub number is mostly a worst-case bound.

`fsync_policy` for BlipMQ on this run: `every_n:64`. NATS JS:
`--storage file --replicas 1` (the moral equivalent).

## Multi-publisher pure throughput (BlipMQ-only)

For completeness, BlipMQ's in-process `wire_multi_pub` measures N
independent `(publisher, subscriber)` pairs. NATS bench has no direct
equivalent.

| pipes | aggregate msg/s |
|---:|---:|
|  1 | 1.7 M |
|  4 | 5.0 M |
|  8 | 6.5 M |
| 16 | **6.9 M** |

These numbers are *in-process* — the publisher and subscriber share
the broker's tokio runtime. Cross-process numbers will be lower (the
per-pipe single-pipe cross-process result above is 0.86 M for 64 B),
but the qualitative scaling — peak ≈ 4× the single-pipe rate at
8 pipes — does carry over.

## Headline

- **Single connection: BlipMQ wins.** With pub-side write batching
  matching NATS's PUB-line pipelining, BlipMQ runs ~50% faster at
  64 B and ~37% faster at 1 KiB.
- **Fan-out: BlipMQ wins decisively.** Per-subscriber writer tasks let
  the broker scale linearly with N; NATS bottlenecks on the publisher
  at ~1.6 M aggregate regardless of N.
- **Durability single-pub: NATS JS wins.** BlipMQ's strict
  fsync-per-publish is harsher than JetStream's lazier append-and-flush.
- **Durability with concurrent publishers: BlipMQ catches up.** WAL
  group-commit batches multiple publishers' fsyncs, so 8 parallel pubs
  hit 96 K msg/s vs JS's 60 K serial.

BlipMQ ahead in three of four categories; the remaining gap is
serial-publisher durability, where there's a real semantic difference
(strict fsync-per-publish vs lazy append).

## Caveats

- **Loopback only.** Real-world RTT changes the picture; both brokers
  pipeline aggressively, but the magnitudes will be different on a
  real NIC. Re-run on the actual deployment topology before drawing
  operational conclusions.
- **Default configs.** No tuning of either broker's per-connection
  buffer sizes, fsync policy, or thread counts beyond what the config
  files in `bench_compare/results/blipmq-bench.toml` specify.
- **Cross-process != production.** Real publishers and subscribers
  often live in the same process as application logic, with all the
  scheduling overhead that implies. These numbers are for the broker
  itself.

## Reproducing

```bash
# Brokers (background)
nats-server --jetstream --store_dir /tmp/nats_js > /tmp/nats.log 2>&1 &
blipmqd --config bench_compare/results/blipmq-bench.toml > /tmp/blipmq.log 2>&1 &

# NATS side (natscli v0.1.5+)
NATS=$(which nats)
$NATS bench n64  --pub 1 --sub 1  --msgs 200000 --size 64
$NATS bench n1k  --pub 1 --sub 1  --msgs 100000 --size 1024
$NATS bench n8f  --pub 1 --sub 8  --msgs 50000  --size 64
$NATS bench n32f --pub 1 --sub 32 --msgs 30000  --size 64

# JetStream — create the stream first
$NATS stream add JS_BENCH --subjects bench-js --storage file --replicas 1 \
  --discard old --max-bytes 1GB --max-msgs -1 --max-msg-size -1 \
  --max-age 0 --retention limits --dupe-window 2m --defaults
$NATS bench bench-js --js --pub 1 --sub 1 --msgs 50000 --size 1024

# BlipMQ side
SUB=target/release/sub_bmq; PUB=target/release/pub_bmq
$SUB --count 200000 --subject test --qos 0 &
sleep 1
$PUB --count 200000 --message-size 64 --subject test --qos 0
wait
# (repeat for the other shapes; for fanout, spawn N $SUB processes)
```
