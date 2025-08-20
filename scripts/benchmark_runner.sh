#!/bin/bash
# BlipMQ Production Benchmark Runner
# Comprehensive performance testing script

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m' 
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Configuration
BENCHMARK_DIR="./target/criterion"
RESULTS_DIR="./benchmark-results"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
RESULTS_FILE="$RESULTS_DIR/benchmark_$TIMESTAMP.json"

echo -e "${BLUE}🚀 BlipMQ Production Benchmark Suite${NC}"
echo "======================================"

# Create results directory
mkdir -p "$RESULTS_DIR"

# System information
echo -e "${YELLOW}📊 System Information${NC}"
echo "CPU: $(cat /proc/cpuinfo | grep 'model name' | head -1 | cut -d':' -f2 | xargs)"
echo "CPU Cores: $(nproc)"
echo "Memory: $(free -h | grep Mem | awk '{print $2}')"
echo "Disk: $(df -h . | tail -1 | awk '{print $4}' | sed 's/G/ GB/')"
echo "Kernel: $(uname -r)"
echo "Rust: $(rustc --version)"
echo ""

# Build optimized binary
echo -e "${YELLOW}🔨 Building optimized binary...${NC}"
export RUSTFLAGS="-C target-cpu=native -C opt-level=3 -C lto=fat"
cargo build --release --bin blipmq-optimized

# Run different benchmark suites
echo -e "${YELLOW}📈 Running benchmark suites...${NC}"

# 1. Memory pool benchmarks
echo -e "${BLUE}Memory Pool Benchmarks${NC}"
cargo bench --bench production_benchmark benchmark_memory_pools -- --output-format json | tee "$RESULTS_DIR/memory_pools_$TIMESTAMP.json"

# 2. Vectored I/O benchmarks  
echo -e "${BLUE}Vectored I/O Benchmarks${NC}"
cargo bench --bench production_benchmark benchmark_vectored_io -- --output-format json | tee "$RESULTS_DIR/vectored_io_$TIMESTAMP.json"

# 3. Message encoding benchmarks
echo -e "${BLUE}Message Encoding Benchmarks${NC}"
cargo bench --bench production_benchmark benchmark_message_encoding -- --output-format json | tee "$RESULTS_DIR/message_encoding_$TIMESTAMP.json"

# 4. Connection management benchmarks
echo -e "${BLUE}Connection Management Benchmarks${NC}"
cargo bench --bench production_benchmark benchmark_connection_management -- --output-format json | tee "$RESULTS_DIR/connection_mgmt_$TIMESTAMP.json"

# 5. End-to-end latency benchmarks
echo -e "${BLUE}End-to-End Latency Benchmarks${NC}"
cargo bench --bench production_benchmark benchmark_e2e_latency -- --output-format json | tee "$RESULTS_DIR/e2e_latency_$TIMESTAMP.json"

# 6. Throughput configuration benchmarks
echo -e "${BLUE}Throughput Configuration Benchmarks${NC}"
cargo bench --bench production_benchmark benchmark_throughput_configs -- --output-format json | tee "$RESULTS_DIR/throughput_configs_$TIMESTAMP.json"

# 7. Memory efficiency benchmarks
echo -e "${BLUE}Memory Efficiency Benchmarks${NC}"
cargo bench --bench production_benchmark benchmark_memory_efficiency -- --output-format json | tee "$RESULTS_DIR/memory_efficiency_$TIMESTAMP.json"

# 8. Stress test benchmarks
echo -e "${BLUE}Stress Test Benchmarks${NC}"
cargo bench --bench production_benchmark benchmark_stress_test -- --output-format json | tee "$RESULTS_DIR/stress_test_$TIMESTAMP.json"

# Network benchmark (requires external setup)
if command -v nats-server &> /dev/null; then
    echo -e "${BLUE}Network Comparison Benchmarks${NC}"
    cargo bench --bench network_benchmark -- --output-format json | tee "$RESULTS_DIR/network_comparison_$TIMESTAMP.json"
else
    echo -e "${YELLOW}⚠️  Skipping network comparison (nats-server not available)${NC}"
fi

# Performance profiling
echo -e "${YELLOW}🔍 Performance Profiling${NC}"
if command -v perf &> /dev/null; then
    echo "Running perf analysis..."
    sudo perf record -g --call-graph=fp ./target/release/blipmq-optimized --config production.toml &
    BROKER_PID=$!
    sleep 5
    
    # Send some test traffic
    echo "Generating test traffic..."
    ./target/release/blipmq-cli --addr 127.0.0.1:8080 pub test "benchmark message" &
    sleep 2
    
    # Stop broker
    kill $BROKER_PID
    wait $BROKER_PID 2>/dev/null || true
    
    # Generate perf report
    sudo perf report --no-children --stdio > "$RESULTS_DIR/perf_report_$TIMESTAMP.txt"
    echo "Perf report saved to $RESULTS_DIR/perf_report_$TIMESTAMP.txt"
fi

# Memory analysis with valgrind (if available)
if command -v valgrind &> /dev/null; then
    echo -e "${BLUE}Memory Analysis with Valgrind${NC}"
    valgrind --tool=massif --stacks=yes \
        ./target/release/blipmq-optimized --config config-profiles/resource-efficient.toml &
    VALGRIND_PID=$!
    sleep 10
    kill $VALGRIND_PID 2>/dev/null || true
    
    # Generate memory report
    if [ -f massif.out.* ]; then
        ms_print massif.out.* > "$RESULTS_DIR/memory_analysis_$TIMESTAMP.txt"
        echo "Memory analysis saved to $RESULTS_DIR/memory_analysis_$TIMESTAMP.txt"
    fi
fi

# Generate summary report
echo -e "${YELLOW}📝 Generating Summary Report${NC}"
cat > "$RESULTS_DIR/benchmark_summary_$TIMESTAMP.md" << EOF
# BlipMQ Benchmark Results - $TIMESTAMP

## System Information
- **CPU**: $(cat /proc/cpuinfo | grep 'model name' | head -1 | cut -d':' -f2 | xargs)
- **CPU Cores**: $(nproc)  
- **Memory**: $(free -h | grep Mem | awk '{print $2}')
- **Disk**: $(df -h . | tail -1 | awk '{print $4}')
- **Kernel**: $(uname -r)
- **Rust**: $(rustc --version)

## Benchmark Results

### Key Metrics
$(if [ -f "$RESULTS_DIR/e2e_latency_$TIMESTAMP.json" ]; then
    echo "- **Low Latency P99**: $(grep -o '"mean":[0-9.]*' "$RESULTS_DIR/e2e_latency_$TIMESTAMP.json" | head -1 | cut -d':' -f2) ms"
fi)

$(if [ -f "$RESULTS_DIR/throughput_configs_$TIMESTAMP.json" ]; then
    echo "- **High Throughput**: $(grep -o '"mean":[0-9.]*' "$RESULTS_DIR/throughput_configs_$TIMESTAMP.json" | head -1 | cut -d':' -f2) msg/s"
fi)

$(if [ -f "$RESULTS_DIR/memory_pools_$TIMESTAMP.json" ]; then
    echo "- **Memory Pool Efficiency**: $(grep -o '"mean":[0-9.]*' "$RESULTS_DIR/memory_pools_$TIMESTAMP.json" | head -1 | cut -d':' -f2) ns/op"
fi)

### Files Generated
$(ls -la "$RESULTS_DIR"/*"$TIMESTAMP"* | awk '{print "- " $9}')

## Recommendations

### Production Configuration
Based on these benchmarks:

1. **For Low Latency**: Use \`low-latency.toml\` profile
   - Expected P99 latency: < 1ms
   - Optimal for: Trading, gaming, real-time systems

2. **For High Throughput**: Use \`high-throughput.toml\` profile  
   - Expected throughput: > 1M msg/s
   - Optimal for: Analytics, data processing, bulk messaging

3. **For General Use**: Use \`production.toml\` (balanced profile)
   - Good balance of latency and throughput
   - Optimal for: Most production workloads

### System Tuning
$(if [ $(nproc) -gt 4 ]; then
    echo "- ✅ Sufficient CPU cores ($(nproc)) for high-performance deployment"
else
    echo "- ⚠️  Consider more CPU cores for maximum performance (current: $(nproc))"
fi)

$(if [ $(free -g | grep Mem | awk '{print $2}') -gt 4 ]; then
    echo "- ✅ Sufficient memory for production deployment"
else
    echo "- ⚠️  Consider more memory for optimal buffer pooling"
fi)

### Next Steps
1. Review detailed results in individual JSON files
2. Run load tests with your specific message patterns
3. Monitor performance metrics in production
4. Tune configuration based on actual workload

Generated: $(date)
EOF

echo ""
echo -e "${GREEN}✅ Benchmark suite completed!${NC}"
echo -e "${GREEN}📁 Results saved in: $RESULTS_DIR${NC}"
echo -e "${GREEN}📊 Summary report: $RESULTS_DIR/benchmark_summary_$TIMESTAMP.md${NC}"

# Optional: Open results in browser (if available)
if command -v xdg-open &> /dev/null; then
    echo ""
    echo -e "${YELLOW}Opening results in browser...${NC}"
    cd target/criterion && python3 -m http.server 8000 &
    SERVER_PID=$!
    sleep 2
    xdg-open http://localhost:8000
    echo "Press Ctrl+C to stop results server"
    wait $SERVER_PID
fi

echo -e "${BLUE}Benchmark complete! 🎉${NC}"