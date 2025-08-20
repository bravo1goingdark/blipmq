//! Production-grade benchmarks for optimized BlipMQ.
//! 
//! Comprehensive performance testing covering:
//! - Memory pool efficiency
//! - Vectored I/O performance  
//! - Connection management
//! - End-to-end message latency
//! - High-throughput scenarios

use blipmq::{
    core::{
        memory_pool::{acquire_small_buffer, acquire_medium_buffer, POOL_STATS},
        performance::{LatencyTimer, PERFORMANCE_METRICS},
        connection_manager::{ConnectionManager, ConnectionLimits},
        vectored_io::{BatchWriter, OptimizedEncoder},
        advanced_config::ProductionConfig,
    },
    BrokerBuilder,
};

use criterion::{
    criterion_group, criterion_main, 
    Criterion, BenchmarkId, Throughput,
    measurement::WallTime,
    BatchSize,
};

use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;
use bytes::{BytesMut, Bytes};

/// Memory pool benchmarks
fn benchmark_memory_pools(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory_pools");
    
    // Buffer pool acquisition/release
    group.bench_function("small_buffer_acquire_release", |b| {
        b.iter(|| {
            let buffer = acquire_small_buffer();
            // Simulate usage
            let _ = buffer.as_ref().len();
            // Buffer automatically returned to pool on drop
        });
    });
    
    group.bench_function("medium_buffer_acquire_release", |b| {
        b.iter(|| {
            let buffer = acquire_medium_buffer();
            let _ = buffer.as_ref().len();
        });
    });
    
    // Compare with standard allocation
    group.bench_function("standard_allocation", |b| {
        b.iter(|| {
            let buffer = BytesMut::with_capacity(2048);
            let _ = buffer.len();
        });
    });
    
    // Batch allocations
    group.bench_function("batch_pool_allocation", |b| {
        b.iter_batched(
            || (0..100).collect::<Vec<_>>(),
            |_items| {
                let _buffers: Vec<_> = (0..100)
                    .map(|_| acquire_small_buffer())
                    .collect();
                // All buffers returned to pool on drop
            },
            BatchSize::SmallInput,
        );
    });
    
    group.finish();
}

/// Vectored I/O benchmarks
fn benchmark_vectored_io(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("vectored_io");
    
    // Batch writing performance
    let message_sizes = [64, 256, 1024, 4096];
    let batch_sizes = [1, 8, 16, 64];
    
    for &msg_size in &message_sizes {
        for &batch_size in &batch_sizes {
            group.throughput(Throughput::Elements(batch_size));
            
            group.bench_with_input(
                BenchmarkId::from_parameter(format!("{}B_{}msgs", msg_size, batch_size)),
                &(msg_size, batch_size),
                |b, &(msg_size, batch_size)| {
                    b.to_async(&rt).iter_batched(
                        || {
                            let messages: Vec<Bytes> = (0..batch_size)
                                .map(|i| Bytes::from(vec![i as u8; msg_size]))
                                .collect();
                            let writer = tokio_test::io::Builder::new().build();
                            let mut batch_writer = BatchWriter::new(writer);
                            (messages, batch_writer)
                        },
                        |(messages, mut batch_writer)| async move {
                            for msg in messages {
                                batch_writer.add_buffer(msg).await.unwrap();
                            }
                            batch_writer.flush_batch().await.unwrap();
                        },
                        BatchSize::SmallInput,
                    );
                },
            );
        }
    }
    
    group.finish();
}

/// Message encoding/decoding benchmarks
fn benchmark_message_encoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("message_encoding");
    
    let message_sizes = [64, 256, 1024, 4096];
    
    for &size in &message_sizes {
        group.throughput(Throughput::Bytes(size as u64));
        
        // Single message encoding
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("single_{}B", size)),
            &size,
            |b, &size| {
                let message = blipmq::core::message::new_message(vec![42u8; size]);
                b.iter(|| {
                    let _encoded = blipmq::core::message::encode_message(&message);
                });
            },
        );
        
        // Batch message encoding
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("batch_{}B", size)),
            &size,
            |b, &size| {
                let messages: Vec<_> = (0..10)
                    .map(|i| blipmq::core::message::new_message(vec![i as u8; size]))
                    .collect();
                let message_refs: Vec<_> = messages.iter().collect();
                
                b.iter(|| {
                    let mut encoder = OptimizedEncoder::new();
                    let _encoded = encoder.encode_message_batch(&message_refs);
                });
            },
        );
    }
    
    group.finish();
}

/// Connection management benchmarks
fn benchmark_connection_management(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("connection_management");
    
    // Connection acceptance rate
    group.bench_function("connection_accept", |b| {
        b.to_async(&rt).iter_batched(
            || {
                let limits = ConnectionLimits::default();
                let manager = ConnectionManager::new(limits);
                manager
            },
            |manager| async move {
                let addr = "127.0.0.1:12345".parse().unwrap();
                let _connection = manager.try_accept_connection(addr).await.unwrap();
            },
            BatchSize::SmallInput,
        );
    });
    
    // Rate limiting performance
    group.bench_function("rate_limit_check", |b| {
        b.to_async(&rt).iter_batched(
            || {
                let limits = ConnectionLimits::default();
                let manager = Arc::new(ConnectionManager::new(limits));
                let addr = "127.0.0.1:12346".parse().unwrap();
                let rt = tokio::runtime::Handle::current();
                let connection = rt.block_on(async {
                    manager.try_accept_connection(addr).await.unwrap()
                });
                (manager, addr, connection)
            },
            |(manager, addr, _connection)| async move {
                let _allowed = manager.check_rate_limit(addr);
            },
            BatchSize::SmallInput,
        );
    });
    
    group.finish();
}

/// End-to-end latency benchmarks
fn benchmark_e2e_latency(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("e2e_latency");
    group.sample_size(1000);
    group.measurement_time(Duration::from_secs(30));
    
    // Low-latency profile
    group.bench_function("low_latency_profile", |b| {
        b.to_async(&rt).iter_batched(
            || {
                let config = ProductionConfig::low_latency_profile();
                let builder = BrokerBuilder::new().with_profile(
                    blipmq::core::advanced_config::PerformanceProfile::LowLatency
                );
                (config, builder)
            },
            |(_config, _builder)| async move {
                // Simulate message processing pipeline
                let timer = LatencyTimer::start();
                
                // Message creation
                let message = blipmq::core::message::new_message(b"test message");
                
                // Encoding
                let _encoded = blipmq::core::message::encode_message(&message);
                
                // Record latency
                timer.record_publish_latency();
            },
            BatchSize::SmallInput,
        );
    });
    
    // High-throughput profile
    group.bench_function("high_throughput_profile", |b| {
        b.to_async(&rt).iter_batched(
            || {
                let messages: Vec<_> = (0..64)
                    .map(|i| blipmq::core::message::new_message(format!("message {}", i)))
                    .collect();
                messages
            },
            |messages| async move {
                let timer = LatencyTimer::start();
                
                // Batch processing
                let mut encoder = OptimizedEncoder::new();
                let message_refs: Vec<_> = messages.iter().collect();
                let _encoded = encoder.encode_message_batch(&message_refs);
                
                timer.record_delivery_latency();
            },
            BatchSize::SmallInput,
        );
    });
    
    group.finish();
}

/// Throughput benchmarks with different configurations
fn benchmark_throughput_configs(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput_configs");
    
    let configs = [
        ("low_latency", ProductionConfig::low_latency_profile()),
        ("high_throughput", ProductionConfig::high_throughput_profile()),
        ("balanced", ProductionConfig::balanced_profile()),
        ("resource_efficient", ProductionConfig::resource_efficient_profile()),
    ];
    
    for (name, config) in &configs {
        group.throughput(Throughput::Elements(1000));
        
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            config,
            |b, config| {
                b.iter_batched(
                    || {
                        let batch_size = config.networking.max_batch_count;
                        let messages: Vec<_> = (0..batch_size)
                            .map(|i| blipmq::core::message::new_message(format!("msg {}", i)))
                            .collect();
                        messages
                    },
                    |messages| {
                        // Simulate processing with configuration parameters
                        let mut encoder = OptimizedEncoder::new();
                        let message_refs: Vec<_> = messages.iter().collect();
                        let _encoded = encoder.encode_message_batch(&message_refs);
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    
    group.finish();
}

/// Memory usage and efficiency benchmarks
fn benchmark_memory_efficiency(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory_efficiency");
    
    // Pool efficiency over time
    group.bench_function("pool_efficiency", |b| {
        b.iter_batched(
            || {
                // Reset pool stats
                POOL_STATS.reset();
                0u32
            },
            |_| {
                // Simulate mixed usage patterns
                let buffers: Vec<_> = (0..50)
                    .map(|_| acquire_small_buffer())
                    .collect();
                
                let hit_rate = POOL_STATS.buffer_hit_rate();
                std::hint::black_box(hit_rate);
                
                // Buffers dropped here, returned to pool
                drop(buffers);
            },
            BatchSize::SmallInput,
        );
    });
    
    // Memory pressure simulation
    group.bench_function("memory_pressure", |b| {
        b.iter_batched(
            || Vec::new(),
            |mut buffers: Vec<_>| {
                // Simulate memory pressure by keeping references
                for _ in 0..20 {
                    buffers.push(acquire_medium_buffer());
                }
                
                // Release half
                buffers.truncate(10);
                
                // Allocate more
                for _ in 0..10 {
                    buffers.push(acquire_small_buffer());
                }
                
                buffers.len()
            },
            BatchSize::SmallInput,
        );
    });
    
    group.finish();
}

/// Comparison with competitors (NATS, Redis) - requires external setup
#[cfg(feature = "competitor_benchmarks")]
fn benchmark_competitor_comparison(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("competitor_comparison");
    group.sample_size(100);
    
    // BlipMQ optimized
    group.bench_function("blipmq_optimized", |b| {
        b.to_async(&rt).iter(|| async {
            // Simulate BlipMQ message processing
            let message = blipmq::core::message::new_message(b"benchmark message");
            let _encoded = blipmq::core::message::encode_message(&message);
        });
    });
    
    // Note: NATS and Redis benchmarks would require running instances
    // This is a placeholder for integration testing
    
    group.finish();
}

/// Stress testing under load
fn benchmark_stress_test(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("stress_test");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(60));
    
    // High connection count
    group.bench_function("high_connection_stress", |b| {
        b.to_async(&rt).iter_batched(
            || {
                let limits = ConnectionLimits {
                    max_connections: 10000,
                    max_connections_per_ip: 1000,
                    ..Default::default()
                };
                ConnectionManager::new(limits)
            },
            |manager| async move {
                // Simulate many connections
                let mut connections = Vec::new();
                for i in 0..100 {
                    let addr = format!("192.168.1.{}", i % 254 + 1).parse().unwrap();
                    if let Ok(conn) = manager.try_accept_connection(addr).await {
                        connections.push(conn);
                    }
                }
                connections.len()
            },
            BatchSize::SmallInput,
        );
    });
    
    // High message throughput
    group.bench_function("high_throughput_stress", |b| {
        b.iter_batched(
            || {
                let messages: Vec<_> = (0..1000)
                    .map(|i| blipmq::core::message::new_message(format!("stress test message {}", i)))
                    .collect();
                messages
            },
            |messages| {
                let mut encoder = OptimizedEncoder::new();
                let message_refs: Vec<_> = messages.iter().collect();
                let _encoded = encoder.encode_message_batch(&message_refs);
            },
            BatchSize::LargeInput,
        );
    });
    
    group.finish();
}

criterion_group!(
    production_benches,
    benchmark_memory_pools,
    benchmark_vectored_io, 
    benchmark_message_encoding,
    benchmark_connection_management,
    benchmark_e2e_latency,
    benchmark_throughput_configs,
    benchmark_memory_efficiency,
    benchmark_stress_test
);

#[cfg(feature = "competitor_benchmarks")]
criterion_group!(
    competitor_benches,
    benchmark_competitor_comparison
);

#[cfg(feature = "competitor_benchmarks")]
criterion_main!(production_benches, competitor_benches);

#[cfg(not(feature = "competitor_benchmarks"))]
criterion_main!(production_benches);