# BlipMQ Configuration

`blipmq_config` defines how configuration is loaded and merged from files and environment variables. This document describes all config keys, their types/defaults, and recommended usage.

## Config Structure

`blipmq_config::Config`:

```rust
pub struct Config {
    pub bind_addr: String,
    pub port: u16,
    pub metrics_addr: String,
    pub metrics_port: u16,
    pub wal_path: String,
    pub fsync_policy: Option<String>,
    pub max_retries: u32,
    pub retry_backoff_ms: u64,
    pub allowed_api_keys: Vec<String>,
    pub enable_tokio_console: bool,
}
```

## Defaults

If no config file is present and no environment variables are set:

- `bind_addr`: `"127.0.0.1"`
- `port`: `5555`
- `metrics_addr`: `"127.0.0.1"`
- `metrics_port`: `9100`
- `wal_path`: `"blipmq.wal"`
- `fsync_policy`: `None` (interpreted via `WalConfig::default()` → `fsync_every_n = Some(64)`)
- `max_retries`: `3`
- `retry_backoff_ms`: `100`
- `allowed_api_keys`: `[]` (no clients can authenticate)
- `enable_tokio_console`: `false`

## File Formats and Merge Strategy

- Config file:
  - TOML (`.toml`) or YAML (`.yaml`, `.yml`).
  - Parsed into an internal `FileConfig` with all fields optional.
- Environment variables:
  - Override file values when present.

Merge rules:

1. File path is chosen from:
   - CLI `--config` parameter, or
   - `BLIPMQ_CONFIG` environment variable, or
   - None (no file).
2. File values are loaded and defaulted as above.
3. Environment variables, if set, override the corresponding file values.

## Key Reference

### `bind_addr` (String)

- Bind address for the TCP broker.
- Example: `"127.0.0.1"`, `"0.0.0.0"`.
- Env override: `BLIPMQ_BIND_ADDR`.

### `port` (u16)

- TCP port for the broker.
- Default: `5555`.
- Env override: `BLIPMQ_PORT`.

### `metrics_addr` (String)

- Bind address for the HTTP metrics endpoint.
- Default: `"127.0.0.1"`.
- Env override: `BLIPMQ_METRICS_ADDR`.

### `metrics_port` (u16)

- HTTP port for metrics.
- Default: `9100`.
- Env override: `BLIPMQ_METRICS_PORT`.

### `wal_path` (String)

- Path to the WAL file.
- Default: `"blipmq.wal"`.
- Env override: `BLIPMQ_WAL_PATH`.

### `fsync_policy` (Option<String>)

- String that controls WAL flush policy.
- Recognized values:
  - `"always"`:
    - fsync on every append.
    - Mapped to `fsync_every_n = Some(1)` and `fsync_interval = None`.
  - `"none"`:
    - No fsync in the WAL layer.
    - Mapped to `fsync_every_n = None`, `fsync_interval = None`.
  - `"every_n:<n>"` (e.g. `"every_n:64"`):
    - fsync after every `n` records.
  - `"interval_ms:<ms>"` (e.g. `"interval_ms:50"`):
    - fsync when at least `<ms>` milliseconds have elapsed since last fsync.
- If not set or unparsable, `WalConfig::default()` is used.
- Env override: `BLIPMQ_FSYNC_POLICY`.

### `max_retries` (u32)

- Maximum QoS1 delivery attempts before dropping a message.
- Default: `3`.
- Env override: `BLIPMQ_MAX_RETRIES`.

### `retry_backoff_ms` (u64)

- Base delay for exponential backoff between QoS1 retries, in milliseconds.
- Effective delay per attempt is based on this base value and attempt count.
- Default: `100`.
- Env override: `BLIPMQ_RETRY_BACKOFF_MS`.

### `allowed_api_keys` (Vec<String>)

- Static API keys allowed for authentication.
- File format: array of strings in TOML/YAML.
- Env override: `BLIPMQ_ALLOWED_API_KEYS`:
  - Comma-separated list: `"key1,key2,key3"`.
  - Whitespace around entries is trimmed.

### `enable_tokio_console` (bool)

- Enables `tokio-console` tracing subscriber instead of default logging.
- Intended for debugging and profiling, not always-on production use.
- Env override: `BLIPMQ_ENABLE_TOKIO_CONSOLE`:
  - Truthy values: `"1"`, `"true"`, `"yes"`, `"on"` (case-insensitive).

## Example: Complete blipmq.toml

`/etc/blipmq/blipmq.toml`:

```toml
# TCP listener for the binary protocol
bind_addr = "0.0.0.0"
port = 5555

# HTTP metrics endpoint
metrics_addr = "0.0.0.0"
metrics_port = 9100

# WAL configuration
wal_path = "/var/lib/blipmq/blipmq.wal"
fsync_policy = "every_n:64" # good balance between durability and throughput

# QoS1 retry and backoff
max_retries = 5
retry_backoff_ms = 50

# Allowed API keys
allowed_api_keys = [
  "prod-key-2025-01-01-A",
  "prod-key-2025-01-01-B",
]

# Disable tokio-console by default in production
enable_tokio_console = false
```

## Example: Matching Env Override Set

To override some file values for a specific deployment:

```bash
export BLIPMQ_CONFIG=/etc/blipmq/blipmq.toml

# Run on port 6000 in this environment
export BLIPMQ_PORT=6000

# Tighter WAL fsync during a critical window
export BLIPMQ_FSYNC_POLICY="interval_ms:20"

# Add a temporary API key for a new client rollout
export BLIPMQ_ALLOWED_API_KEYS="prod-key-2025-01-01-A,prod-key-2025-01-01-B,rollout-key"

# Enable tokio-console during an investigation
export BLIPMQ_ENABLE_TOKIO_CONSOLE=true

blipmqd
```

## Production Recommendations

- **Always define `allowed_api_keys`**:
  - An empty list means no client can authenticate.
  - Use long, random keys managed via a secrets system.
- **Place WAL on reliable storage**:
  - Use a dedicated SSD or high-performance disk.
  - Configure filesystem and permissions so that only the `blipmq` user can access it.
- **Tune `fsync_policy` for your durability/latency tradeoff**:
  - `always`: maximum durability, lowest throughput.
  - `every_n:64` or `interval_ms:20`: good defaults for many deployments.
  - `none`: only acceptable if additional redundancy exists and some loss is acceptable.
- **Calibrate `max_retries` and `retry_backoff_ms`**:
  - Larger `max_retries` and shorter backoffs increase resilience but can amplify load during outages.
- **Use `enable_tokio_console` selectively**:
  - Enable only when actively inspecting performance or correctness issues.
  - Leave disabled in steady-state production to reduce overhead.
