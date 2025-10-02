# BlipMQ Performance Tuning Guide

## Overview
BlipMQ has been optimized for high-throughput, low-latency message delivery. This guide covers the performance optimizations implemented and how to tune the system for your specific workload.

## Key Performance Optimizations

### 1. Memory Management
- **Mimalloc Allocator**: Enabled by default for better allocation performance
- **Pre-sized Collections**: All major data structures are pre-allocated to reduce reallocation overhead
- **Buffer Reuse**: Message buffers are reused across flush cycles to minimize allocations

### 2. Network I/O
- **TCP Optimizations**:
  - TCP_NODELAY enabled (Nagle's algorithm disabled) for lower latency
  - Large socket buffers (512KB) for better throughput
  - Increased initial buffer sizes (16KB for commands, 4KB for frames)

### 3. Concurrency & Parallelism
- **Automatic Sharding**: Topics use 2x CPU core count for shards by default
- **CPU Affinity**: Workers are pinned to specific CPU cores (Windows)
- **Lock-free Queues**: Using crossbeam's ArrayQueue for SPSC communication
- **DashMap**: Concurrent hash maps for subscriber management

### 4. Message Processing
- **Batch Processing**: Messages are processed in batches of up to 256 messages
- **Early TTL Drops**: Expired messages are dropped before enqueueing
- **Zero-copy Frames**: Arc<WireMessage> shared across subscribers

## Configuration Tuning

### High-Throughput Settings
```toml
[server]
max_connections = 10000
max_message_size_bytes = 4194304  # 4MB

[queues]
topic_capacity = 50000      # Very large topic buffers
subscriber_capacity = 8192   # Large subscriber queues

[delivery]
max_batch = 512             # Maximum batching
fanout_shards = 0           # Auto-detect (2x CPU cores)
```

### Low-Latency Settings
```toml
[queues]
topic_capacity = 1000       # Smaller buffers
subscriber_capacity = 256    # Smaller queues

[delivery]
max_batch = 32              # Smaller batches
fanout_shards = 8           # Fixed shard count
```

### Mixed Workload (Default)
```toml
[queues]
topic_capacity = 10000
subscriber_capacity = 2048

[delivery]
max_batch = 256
fanout_shards = 0           # Auto-detect
```

## Build Optimizations

### Release Build
```bash
cargo build --release
```

### Performance Build (with debug symbols)
```bash
cargo build --profile=perf
```

### Compilation Features
- `mimalloc`: High-performance allocator (enabled by default)
- `affinity`: CPU core pinning (Windows, enabled by default)

## Benchmarking

### Running Benchmarks
```bash
# Quick benchmark
BLIPMQ_QUICK_BENCH=1 cargo bench

# Full benchmark
cargo bench
```

### Expected Performance
On modern hardware (8+ cores, NVMe SSD):
- **Throughput**: 1M+ messages/second (fanout to 48 subscribers)
- **Latency**: P50 < 100µs, P99 < 1ms
- **Memory**: ~100MB base + 1KB per subscriber

## System Tuning

### Linux
```bash
# Increase file descriptor limits
ulimit -n 65536

# TCP tuning
echo "net.core.rmem_max = 134217728" >> /etc/sysctl.conf
echo "net.core.wmem_max = 134217728" >> /etc/sysctl.conf
echo "net.ipv4.tcp_rmem = 4096 87380 134217728" >> /etc/sysctl.conf
echo "net.ipv4.tcp_wmem = 4096 65536 134217728" >> /etc/sysctl.conf
sysctl -p
```

### Windows
```powershell
# Run as Administrator
# Increase ephemeral port range
netsh int ipv4 set dynamic tcp start=10000 num=55536

# Disable TCP auto-tuning (for consistent latency)
netsh int tcp set global autotuninglevel=disabled
```

## Monitoring

### Metrics Endpoint
BlipMQ exposes metrics at `http://127.0.0.1:9090` by default:
- Messages published/enqueued/dropped
- Flush operations and bytes
- TTL drops and queue overflows

### Performance Profiling
For detailed profiling, use:
- **Linux**: `perf`, `flamegraph`
- **Windows**: Windows Performance Toolkit, PerfView
- **Cross-platform**: `cargo-flamegraph`

```bash
# Generate flamegraph
cargo install flamegraph
cargo flamegraph --bin blipmq
```

## Troubleshooting

### High CPU Usage
- Check `fanout_shards` - too many shards can cause overhead
- Verify message TTLs - expired messages still consume CPU
- Monitor batch sizes - very small batches increase overhead

### High Memory Usage
- Reduce `topic_capacity` and `subscriber_capacity`
- Check for slow subscribers causing queue buildup
- Enable `drop_new` overflow policy for bounded memory

### Poor Latency
- Reduce `max_batch` for lower batching delay
- Ensure TCP_NODELAY is working (check with Wireshark)
- Consider using fewer shards for better cache locality

## Advanced Optimizations

### NUMA Awareness
For multi-socket systems, consider running multiple BlipMQ instances with CPU affinity:
```bash
# Instance 1 on NUMA node 0
numactl --cpunodebind=0 --membind=0 ./blipmq --config node0.toml

# Instance 2 on NUMA node 1
numactl --cpunodebind=1 --membind=1 ./blipmq --config node1.toml
```

### Huge Pages (Linux)
```bash
# Enable transparent huge pages
echo always > /sys/kernel/mm/transparent_hugepage/enabled
```

### Core Isolation (Linux)
```bash
# Isolate cores 4-7 for BlipMQ
echo "isolcpus=4-7" >> /etc/default/grub
update-grub
reboot

# Run BlipMQ on isolated cores
taskset -c 4-7 ./blipmq
```
