#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-/results}"
MESSAGES="${MESSAGES:-10000}"
SUBJECT="${SUBJECT:-bench}"
NATS_URL="${NATS_URL:-nats://127.0.0.1:4222}"
BLIP_ADDR="${BLIP_ADDR:-127.0.0.1:5555}"
JETSTREAM_FLAG="${JETSTREAM_FLAG:---jetstream}"

mkdir -p "$RESULTS_DIR"

echo "[run_all] Starting NATS with JetStream..."
nats-server --jetstream --store_dir "$RESULTS_DIR/nats_js" >"$RESULTS_DIR/nats.log" 2>&1 &
NATS_PID=$!

echo "[run_all] Writing BlipMQ bench config..."
cat >"$RESULTS_DIR/blipmq-bench.toml" <<'EOF'
bind_addr = "127.0.0.1"
port = 5555
metrics_addr = "127.0.0.1"
metrics_port = 9100
wal_path = "blipmq.wal"
fsync_policy = "every_n:64"
max_retries = 3
retry_backoff_ms = 100
allowed_api_keys = ["bench-key"]
EOF

echo "[run_all] Starting BlipMQ..."
cargo run -p blipmqd --release -- --config "$RESULTS_DIR/blipmq-bench.toml" >"$RESULTS_DIR/blipmq.log" 2>&1 &
BLIP_PID=$!

cleanup() {
  echo "[run_all] Shutting down brokers..."
  kill "$NATS_PID" "$BLIP_PID" 2>/dev/null || true
}
trap cleanup EXIT

sleep 2

echo "[run_all] Running full comparison matrix..."
cargo run -p bench_compare --release -- --broker all ${JETSTREAM_FLAG} \
  --nats-url "$NATS_URL" \
  --blipmq-addr "$BLIP_ADDR" \
  --subject "$SUBJECT" \
  --messages "$MESSAGES" \
  --results-dir "$RESULTS_DIR" \
  --nats-pid "$NATS_PID" \
  --blipmq-pid "$BLIP_PID"

echo "[run_all] Results stored under $RESULTS_DIR"
