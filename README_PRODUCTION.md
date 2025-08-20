# BlipMQ Production Deployment Guide

This document covers deploying BlipMQ in production environments with optimized performance configurations.

## Quick Start

### 1. Using Docker Compose (Recommended)

```bash
# Start with monitoring stack
docker-compose -f docker-compose.production.yml up -d

# Access endpoints
curl http://localhost:8080  # Load balanced BlipMQ
curl http://localhost:9092  # Prometheus metrics
curl http://localhost:3000  # Grafana dashboard (admin/admin123)
curl http://localhost:8404  # HAProxy stats
```

### 2. Native Binary

```bash
# Build optimized binary
cargo build --release --bin blipmq-optimized

# Start with balanced profile (default)
./target/release/blipmq-optimized

# Or with specific profile
./target/release/blipmq-optimized --profile low-latency
./target/release/blipmq-optimized --profile high-throughput --max-connections 50000
```

## Performance Profiles

### Low Latency Profile
- **Target**: < 1ms latency
- **Use Cases**: Trading, gaming, real-time systems
- **Config**: `config-profiles/low-latency.toml`

**Optimizations:**
- TCP_NODELAY enabled
- Small batch sizes (4 messages)
- Thread affinity enabled
- Memory pooling optimized
- Zero-copy I/O
- Minimal logging

### High Throughput Profile
- **Target**: Millions of messages/second
- **Use Cases**: Analytics, data pipelines, bulk processing
- **Config**: `config-profiles/high-throughput.toml`

**Optimizations:**
- Large batch sizes (64+ messages)
- Vectored I/O enabled
- Large buffer pools
- Compression enabled
- Multiple worker threads

### Balanced Profile
- **Target**: General production use
- **Use Cases**: Most applications
- **Config**: `production.toml`

**Optimizations:**
- Moderate batch sizes (16 messages)
- Adaptive memory pooling
- Standard monitoring
- Security enabled

### Resource Efficient Profile
- **Target**: Minimal resource usage
- **Use Cases**: Cloud containers, edge deployment
- **Config**: `config-profiles/resource-efficient.toml`

**Optimizations:**
- Small memory footprint
- Conservative connection limits
- Compression enabled
- Minimal logging

## Configuration

### Environment Variables

```bash
export BLIPMQ_PROFILE=low-latency
export BLIPMQ_BIND_ADDR=0.0.0.0:8080
export BLIPMQ_METRICS_ADDR=127.0.0.1:9090
export RUST_LOG=info
```

### Auto-Tuning

BlipMQ automatically tunes configuration based on system capabilities:

```bash
blipmq-optimized --auto-tune --profile balanced
```

**Auto-tuned parameters:**
- Worker thread count (based on CPU cores)
- Memory limits (based on available RAM)
- Connection limits (based on file descriptors)
- Buffer sizes (based on memory)

### Validation

Validate configuration before deployment:

```bash
blipmq-optimized --config production.toml --validate-config
```

Generate configuration template:

```bash
blipmq-optimized --profile low-latency --print-config > my-config.toml
```

## Production Monitoring

### Metrics Endpoints

- **Prometheus**: `http://host:9090/metrics`
- **JSON Status**: `http://host:9090/status`
- **Health Check**: `http://host:9090/health`

### Key Metrics

| Metric | Description | Alert Threshold |
|--------|-------------|----------------|
| `blipmq_latency_p99_milliseconds` | 99th percentile latency | > 100ms |
| `blipmq_throughput_messages_per_second` | Current throughput | < 100 msg/s |
| `blipmq_connections_active` | Active connections | > 80% of limit |
| `blipmq_memory_used_bytes` | Memory usage | > 2GB |
| `blipmq_buffer_pool_hit_rate` | Buffer pool efficiency | < 80% |
| `blipmq_messages_dropped_total` | Dropped messages | > 0 |

### Grafana Dashboards

Pre-built dashboards available in `monitoring/grafana/dashboards/`:
- **Overview**: High-level system metrics
- **Performance**: Latency and throughput analysis  
- **Resources**: Memory, CPU, connections
- **Alerting**: Current alert status

## Performance Tuning

### System-Level Optimizations

```bash
# Increase file descriptor limits
ulimit -n 65536

# Network optimizations
echo 'net.core.somaxconn = 4096' >> /etc/sysctl.conf
echo 'net.core.netdev_max_backlog = 5000' >> /etc/sysctl.conf
echo 'net.ipv4.tcp_max_syn_backlog = 4096' >> /etc/sysctl.conf

# Memory optimizations  
echo 'vm.swappiness = 1' >> /etc/sysctl.conf
echo 'vm.dirty_ratio = 15' >> /etc/sysctl.conf

# Apply changes
sysctl -p
```

### CPU Affinity

For low-latency workloads, pin BlipMQ to specific CPU cores:

```bash
# Start with CPU affinity
taskset -c 0-3 blipmq-optimized --profile low-latency
```

### Memory Tuning

For high-throughput workloads:

```toml
[memory]
memory_limit_mb = 8192
small_buffer_pool_size = 500
medium_buffer_pool_size = 200
large_buffer_pool_size = 100
enable_zero_copy = true
```

## Deployment Patterns

### High Availability

```yaml
# docker-compose with 3 replicas
services:
  blipmq-1:
    image: blipmq:optimized
    # ... config
  blipmq-2:
    image: blipmq:optimized  
    # ... config
  blipmq-3:
    image: blipmq:optimized
    # ... config
    
  load-balancer:
    image: haproxy
    # Configure all 3 backends
```

### Kubernetes Deployment

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: blipmq
spec:
  replicas: 3
  selector:
    matchLabels:
      app: blipmq
  template:
    spec:
      containers:
      - name: blipmq
        image: blipmq:optimized
        env:
        - name: BLIPMQ_PROFILE
          value: "balanced"
        resources:
          requests:
            memory: "1Gi"
            cpu: "1000m"
          limits:
            memory: "4Gi" 
            cpu: "4000m"
```

## Troubleshooting

### Performance Issues

1. **High Latency**
   ```bash
   # Check system load
   top
   
   # Check network interfaces
   netstat -i
   
   # Review configuration
   blipmq-optimized --validate-config
   ```

2. **Low Throughput** 
   ```bash
   # Check buffer pool efficiency
   curl http://localhost:9090/metrics | grep buffer_pool_hit_rate
   
   # Increase batch sizes
   # Edit config: max_batch_count = 32
   ```

3. **Memory Issues**
   ```bash
   # Monitor memory usage
   curl http://localhost:9090/status | jq '.performance.memory_used_bytes'
   
   # Check for leaks
   valgrind --leak-check=full ./blipmq-optimized
   ```

### Common Alerts

| Alert | Cause | Solution |
|-------|-------|----------|
| High Latency | CPU overload, network issues | Scale up, optimize network |
| Memory Usage | Buffer pool sizing, leaks | Tune pool sizes, restart |
| Connection Limit | Too many clients | Increase limits, add instances |
| Message Drops | Queue overflow | Increase capacity, add consumers |

## Security Considerations

### Production Hardening

1. **Authentication**
   ```toml
   [security]
   enable_authentication = true
   api_keys = ["secure-random-key-here"]
   ```

2. **Rate Limiting**
   ```toml
   [security]
   enable_rate_limiting = true
   rate_limit_per_connection = 1000
   rate_limit_per_ip = 5000
   ```

3. **Network Security**
   - Use private networks
   - Configure firewalls
   - Enable TLS for sensitive data

4. **Container Security**
   ```dockerfile
   # Run as non-root user
   USER blipmq
   
   # Read-only filesystem
   RUN chmod -R 755 /app
   ```

## Backup and Recovery

### Write-Ahead Log Backup

```bash
# Backup WAL directory
tar -czf blipmq-wal-backup.tar.gz ./wal/

# Recovery
tar -xzf blipmq-wal-backup.tar.gz
blipmq-optimized --config production.toml
```

### Configuration Backup

```bash
# Backup all configuration
tar -czf blipmq-config-backup.tar.gz \
  production.toml \
  config-profiles/ \
  monitoring/
```

## Support and Monitoring

### Log Analysis

```bash
# Real-time monitoring
tail -f /app/logs/blipmq.log | grep ERROR

# Performance analysis
grep "latency" /app/logs/blipmq.log | tail -100
```

### Health Checks

```bash
# Kubernetes health check
curl http://blipmq:9090/health

# Load balancer health check  
curl http://load-balancer:8404/stats
```

For additional support and advanced configuration options, visit the [BlipMQ Documentation](https://blipmq.dev/docs).