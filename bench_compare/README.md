# Bench Compare (BlipMQ vs NATS)

This crate brings a reusable benchmark matrix to compare BlipMQ against NATS (core and JetStream). It ships role-specific clients, an orchestrating harness that explores a full parameter grid, automation scripts, and quick plotting utilities.

## Prerequisites

- Rust toolchain (stable).
- `nats-server` available on `PATH` (JetStream enabled for QoS1 comparisons).
- Python 3 with `matplotlib` for plotting (`pip install matplotlib`).

## Harness: full matrix

Runs QoS0 (BlipMQ vs NATS core) and optionally QoS1 (BlipMQ vs NATS JetStream) across message sizes `100B/1KB/4KB` with publisher/subscriber fan-outs of `1/4/16`.

Examples:

- All brokers, including JetStream, writing results under `bench_compare/results`:
  ```bash
  cargo run -p bench_compare --release -- --broker all --jetstream --messages 20000
  ```
- Only NATS core:
  ```bash
  cargo run -p bench_compare -- --broker nats --nats-url nats://127.0.0.1:4222
  ```
- Only BlipMQ QoS0/QoS1 against a specific daemon:
  ```bash
  cargo run -p bench_compare -- --broker blipmq --blipmq-addr 127.0.0.1:5555 --blipmq-api-key bench-key
  ```

Resource monitoring (CPU/memory) uses either supplied PIDs or process names. Defaults: `nats-server` and `blipmqd`. Override with `--nats-pid`, `--blipmq-pid`, `--nats-process`, or `--blipmq-process`.

### Sample harness output

```
Running NATS core: size=1024 pubs=4 subs=4 msgs=20000
nats core    size=1024 pubs=4 subs=4 msgs=20000 thr=122000 msg/s p50=240.12us p95=460.72us p99=721.30us cpu=36.4% mem=8896512 bytes
Running BlipMQ QoS1: size=1024 pubs=4 subs=4 msgs=20000
blipmq qos1  size=1024 pubs=4 subs=4 msgs=20000 thr=131000 msg/s p50=180.44us p95=395.28us p99=612.66us cpu=41.2% mem=12550144 bytes
Results saved to bench_compare/results/results.csv and bench_compare/results/results.json
```

## Standalone clients

Each client honors `--count`, `--message-size`, `--subject`, `--parallelism`, and broker-specific options. Publish clients record publish-call latency; subscribe clients compute end-to-end latency using embedded send timestamps.

- Publish to NATS:
  ```bash
  cargo run -p bench_compare --bin pub_nats -- --count 50000 --parallelism 4 --subject bench --message-size 1024
  ```
- Subscribe from NATS:
  ```bash
  cargo run -p bench_compare --bin sub_nats -- --count 50000 --parallelism 4 --subject bench
  ```
- Publish to BlipMQ:
  ```bash
  cargo run -p bench_compare --bin pub_bmq -- --addr 127.0.0.1:5555 --api-key bench-key --count 50000 --parallelism 4 --message-size 1024 --qos 1
  ```
- Subscribe from BlipMQ:
  ```bash
  cargo run -p bench_compare --bin sub_bmq -- --addr 127.0.0.1:5555 --api-key bench-key --count 50000 --parallelism 4 --subject bench --qos 1
  ```

## Automation

- `scripts/run_all.sh` starts NATS (JetStream) and BlipMQ with a generated config, runs the full harness, and leaves logs plus `results.csv`/`results.json` under `/results` (override via `RESULTS_DIR`). Environment variables: `RESULTS_DIR`, `MESSAGES`, `SUBJECT`, `NATS_URL`, `BLIP_ADDR`, `JETSTREAM_FLAG`.
- `scripts/plot.py` renders PNGs from `results.csv`:
  ```bash
  python bench_compare/scripts/plot.py --results bench_compare/results/results.csv --outdir bench_compare/results
  ```
  Expected outputs: `throughput.png`, `latency_p95.png`, `latency_p99.png`.

## Reading results

Both `results.csv` and `results.json` contain:

- `broker`, `qos`, `message_size`, `publishers`, `subscribers`, `total_messages`
- `throughput_mps`, `latency_p50_us`, `latency_p95_us`, `latency_p99_us`
- `cpu_percent`, `memory_bytes`

CPU is averaged during the run of each scenario; memory is the maximum RSS across samples.
