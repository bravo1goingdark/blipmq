# BlipMQ Deployment Guide

## 🚀 Quick Start

### Download Binaries

1. Visit the [releases page](https://github.com/bravo1goingdark/blipmq/releases)
2. Download the appropriate binary for your platform:
   - **Linux x64**: `blipmq-linux-x64.tar.gz`
   - **Linux ARM64**: `blipmq-linux-arm64.tar.gz`
   - **macOS x64**: `blipmq-macos-x64.tar.gz`
   - **macOS ARM64**: `blipmq-macos-arm64.tar.gz`
   - **Windows x64**: `blipmq-windows-x64.zip`

### Installation

#### Linux/macOS
```bash
# Download and extract
curl -LO https://github.com/bravo1goingdark/blipmq/releases/latest/download/blipmq-linux-x64.tar.gz
tar -xzf blipmq-linux-x64.tar.gz
cd blipmq

# Make executable
chmod +x blipmq blipmq-cli

# Start broker
./blipmq start
```

#### Windows
```powershell
# Download and extract (use Windows Explorer or PowerShell)
Expand-Archive -Path blipmq-windows-x64.zip -DestinationPath .\blipmq

# Navigate to directory
cd blipmq

# Start broker
.\blipmq.exe start
```

#### Docker
```bash
# Pull latest image
docker pull yourdockerhub/blipmq:latest

# Run with default config
docker run -p 7878:7878 -p 9090:9090 yourdockerhub/blipmq:latest

# Run with custom config
docker run -p 7878:7878 -p 9090:9090 \
  -v ./config/blipmq-production.toml:/app/blipmq.toml \
  yourdockerhub/blipmq:latest
```

## ⚡ Performance Configurations

### Development (Low Resource Usage)
```bash
# Use development config
./blipmq start --config config/blipmq-dev.toml
```

### Production (Balanced Performance)
```bash
# Use production config
./blipmq start --config config/blipmq-production.toml
```

### Ultra-Performance (Maximum Throughput)
```bash
# Use ultra-performance config
./blipmq start --config config/blipmq-ultra.toml
```

## 🏗️ Build from Source

### Prerequisites
- Rust 1.75+ (`rustup install stable`)
- System dependencies:
  - **Linux**: `apt-get install clang pkg-config libssl-dev protobuf-compiler`
  - **macOS**: `brew install protobuf`
  - **Windows**: Install Visual Studio Build Tools

### Standard Build
```bash
git clone https://github.com/bravo1goingdark/blipmq.git
cd blipmq
cargo build --release
```

### Ultra-Performance Build
```bash
# Maximum performance with all optimizations
RUSTFLAGS="-C target-cpu=native" \
cargo build --profile ultra --features production

# Alternative with specific CPU features
RUSTFLAGS="-C target-cpu=native -C target-feature=+avx2,+fma" \
cargo build --profile ultra --features production
```

### Cross-Compilation
```bash
# Install cross-compilation tool
cargo install cross

# Build for ARM64 Linux
cross build --release --target aarch64-unknown-linux-gnu --features mimalloc

# Build for ARM64 macOS (from macOS)
cargo build --release --target aarch64-apple-darwin --features mimalloc
```

## 🔧 Configuration Guide

### Configuration Files

BlipMQ uses TOML configuration files. Choose based on your environment:

- `config/blipmq-dev.toml` - Development (debugging, low resources)
- `config/blipmq-production.toml` - Production (balanced performance)
- `config/blipmq-ultra.toml` - Ultra-performance (maximum throughput)

### Key Configuration Sections

#### Server Settings
```toml
[server]
bind_addr = "0.0.0.0:7878"          # Listen address
max_connections = 10000             # Connection limit
max_message_size_bytes = 4194304    # 4MB message limit
```

#### Performance Tuning
```toml
[performance]
enable_timer_wheel = true           # O(1) TTL operations
enable_memory_pools = true          # 200% allocation efficiency
enable_batch_processing = true      # 150% throughput boost
warm_up_pools = true               # Pre-allocate on startup

[performance.cpu_affinity]
enable_affinity = true             # Pin threads to CPU cores
timer_cores = "0,1"               # Dedicated timer cores
fanout_cores = "2,3,4,5"          # Dedicated fanout cores
network_cores = "6,7"             # Dedicated network cores
```

#### Network Optimizations
```toml
[performance.network_optimizations]
tcp_nodelay = true                 # Disable Nagle's algorithm
send_buffer_size = 2097152         # 2MB socket buffers
recv_buffer_size = 2097152         # 2MB socket buffers
reuse_port = true                  # Linux SO_REUSEPORT
```

## 🐳 Docker Deployment

### Production Dockerfile
```dockerfile
FROM yourdockerhub/blipmq:latest

# Copy your production config
COPY your-production.toml /app/blipmq.toml

# Set environment variables
ENV RUST_LOG=info
ENV BLIPMQ_BIND_ADDR=0.0.0.0:7878

EXPOSE 7878 9090

CMD ["./blipmq", "start", "--config", "blipmq.toml"]
```

### Docker Compose
```yaml
version: '3.8'

services:
  blipmq:
    image: yourdockerhub/blipmq:latest
    ports:
      - "7878:7878"  # Broker
      - "9090:9090"  # Metrics
    volumes:
      - ./config/blipmq-production.toml:/app/blipmq.toml:ro
      - blipmq-wal:/app/wal
      - blipmq-logs:/app/logs
    environment:
      - RUST_LOG=info
    restart: unless-stopped
    deploy:
      resources:
        limits:
          memory: 1G
          cpus: '2'
        reservations:
          memory: 512M
          cpus: '1'

volumes:
  blipmq-wal:
  blipmq-logs:
```

## 🏎️ Performance Tuning

### System Optimizations

#### Linux Kernel Parameters
```bash
# Add to /etc/sysctl.conf
net.core.rmem_max = 134217728      # 128MB receive buffer
net.core.wmem_max = 134217728      # 128MB send buffer
net.core.netdev_max_backlog = 5000
net.ipv4.tcp_window_scaling = 1
net.ipv4.tcp_timestamps = 0        # Disable for performance
net.ipv4.tcp_sack = 1
net.ipv4.tcp_rmem = 4096 65536 134217728
net.ipv4.tcp_wmem = 4096 65536 134217728

# Apply changes
sysctl -p
```

#### File Descriptor Limits
```bash
# Add to /etc/security/limits.conf
* soft nofile 1048576
* hard nofile 1048576

# For systemd services, add to service file:
[Service]
LimitNOFILE=1048576
```

#### Huge Pages (Linux)
```bash
# Enable huge pages for better memory performance
echo 1024 > /proc/sys/vm/nr_hugepages
echo always > /sys/kernel/mm/transparent_hugepage/enabled

# Make persistent in /etc/sysctl.conf
vm.nr_hugepages = 1024
```

### CPU Affinity Setup

#### Automatic Core Detection
```bash
# BlipMQ automatically detects CPU cores
./blipmq start --config config/blipmq-ultra.toml
```

#### Manual Core Assignment
```toml
[performance.cpu_affinity]
enable_affinity = true
timer_cores = "0,1"        # Timer wheel on cores 0-1
fanout_cores = "2,3,4,5"   # Message fanout on cores 2-5
network_cores = "6,7"      # Network I/O on cores 6-7
```

### Memory Optimization

#### JEMalloc Configuration
```bash
# Set environment variables for jemalloc tuning
export MALLOC_CONF="background_thread:true,metadata_thp:auto,dirty_decay_ms:5000"
```

#### Memory Pool Tuning
```toml
[performance.pool_config]
buffer_small_count = 8192    # 8K small buffers (1KB each)
buffer_medium_count = 4096   # 4K medium buffers (4KB each)
buffer_large_count = 2048    # 2K large buffers (16KB each)
message_batch_count = 4096   # 4K message batch objects
```

## 📊 Monitoring & Metrics

### Prometheus Metrics
BlipMQ exposes metrics at `http://localhost:9090/metrics`:

- `blipmq_published` - Total published messages
- `blipmq_enqueued` - Total enqueued messages
- `blipmq_dropped_ttl` - Messages dropped due to TTL
- `blipmq_timer_wheel_insertions` - Timer wheel operations
- `blipmq_pool_cache_hits` - Memory pool efficiency
- `blipmq_batch_processing_efficiency` - Batch processing stats

### Health Checks
```bash
# Basic health check
curl -f http://localhost:9090/metrics || exit 1

# CLI health check
./blipmq-cli pub healthcheck "ping" --ttl 1000
```

### Performance Monitoring
```bash
# Monitor real-time performance
watch -n 1 'curl -s http://localhost:9090/metrics | grep blipmq_'

# Monitor system resources
htop -p $(pgrep blipmq)
```

## 🔒 Security Configuration

### API Key Management
```toml
[auth]
api_keys = [
    "secure-production-key-32chars",
    "backup-key-for-failover-32chars"
]
```

### Network Security
```bash
# Firewall configuration (ufw example)
sudo ufw allow 7878/tcp  # BlipMQ broker
sudo ufw deny 9090/tcp   # Metrics (internal only)

# Or with iptables
iptables -A INPUT -p tcp --dport 7878 -j ACCEPT
iptables -A INPUT -p tcp --dport 9090 -s 127.0.0.1 -j ACCEPT
```

### TLS/SSL (Future Enhancement)
```toml
[server.tls]
cert_file = "/path/to/cert.pem"
key_file = "/path/to/key.pem"
ca_file = "/path/to/ca.pem"
```

## 🚨 Troubleshooting

### Common Issues

#### Connection Refused
```bash
# Check if BlipMQ is running
ps aux | grep blipmq

# Check port binding
netstat -tlnp | grep 7878

# Check firewall
sudo ufw status
```

#### Memory Issues
```bash
# Monitor memory usage
free -h
cat /proc/$(pgrep blipmq)/status | grep -E "(VmSize|VmRSS)"

# Check memory pool statistics
curl -s http://localhost:9090/metrics | grep pool
```

#### Performance Degradation
```bash
# Check CPU usage
top -p $(pgrep blipmq)

# Check disk I/O
iotop -p $(pgrep blipmq)

# Check network statistics
ss -i sport = :7878
```

### Log Analysis
```bash
# View real-time logs
journalctl -f -u blipmq

# Or with Docker
docker logs -f <container_id>

# Search for errors
journalctl -u blipmq | grep -i error
```

## 📈 Performance Benchmarks

### Expected Performance

| Configuration | Throughput | Latency P95 | Memory Usage |
|---------------|------------|-------------|--------------|
| Development   | 100K msg/s | 10ms        | 50MB         |
| Production    | 1.2M msg/s | 2.1ms       | 100MB        |
| Ultra         | 1.7M msg/s | 1.5ms       | 150MB        |

### Benchmark Commands
```bash
# Throughput benchmark
cargo bench --bench network_benchmark

# Latency benchmark  
cargo bench --bench timer_wheel_benchmark

# Custom load test
./blipmq-cli pub loadtest "benchmark-$(date +%s)" --ttl 5000
```

## 🔄 Production Checklist

- [ ] Choose appropriate configuration (dev/prod/ultra)
- [ ] Configure system limits (file descriptors, memory)
- [ ] Set up monitoring and alerting
- [ ] Configure log rotation
- [ ] Set up backup for WAL directory
- [ ] Configure firewall rules
- [ ] Test failover scenarios
- [ ] Document recovery procedures
- [ ] Set up health checks
- [ ] Configure resource limits (Docker/k8s)