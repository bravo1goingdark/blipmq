//! Ultra-performance benchmark for BlipMQ
//! 
//! This benchmark tests the new ultra-fast implementation against the original
//! to measure the performance improvements.

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use std::time::{Duration, Instant};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use bytes::Bytes;

use blipmq::core::protocol::{FrameBuilder, generate_message_id, fast_timestamp};
use blipmq::core::lockfree::MpmcQueue;

/// Benchmark the new ultra-fast protocol vs FlatBuffers
fn benchmark_protocol_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol_parsing");
    
    // Test data
    let topic = "test/topic";
    let payload = b"Hello, ultra-fast world!";
    let ttl_ms = 60000u32;
    
    // Benchmark new ultra-fast protocol
    group.bench_function("ultra_fast_protocol", |b| {
        let mut frame_builder = FrameBuilder::new();
        b.iter(|| {
            let frame = frame_builder.publish(topic.as_bytes(), payload, ttl_ms);
            black_box(frame);
        });
    });
    
    // Benchmark FlatBuffers (simulated)
    group.bench_function("flatbuffers_protocol", |b| {
        b.iter(|| {
            // Simulate FlatBuffers overhead
            let mut data = Vec::with_capacity(100);
            data.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            data.extend_from_slice(&(topic.len() as u32).to_be_bytes());
            data.extend_from_slice(&ttl_ms.to_be_bytes());
            data.extend_from_slice(topic.as_bytes());
            data.extend_from_slice(payload);
            data.extend_from_slice(&[0u8; 20]); // FlatBuffers padding
            black_box(data);
        });
    });
    
    group.finish();
}

/// Benchmark lock-free queue vs standard channels
fn benchmark_message_queues(c: &mut Criterion) {
    let mut group = c.benchmark_group("message_queues");
    
    // Test data
    let test_message = Bytes::from("test message");
    let num_messages = 10000;
    
    // Benchmark lock-free MPMC queue
    group.bench_function("lockfree_mpmc_queue", |b| {
        let queue = Arc::new(MpmcQueue::new(65536));
        let counter = Arc::new(AtomicU64::new(0));
        
        // Spawn producer
        let queue_prod = queue.clone();
        let counter_prod = counter.clone();
        let producer = thread::spawn(move || {
            for _ in 0..num_messages {
                while queue_prod.try_enqueue(test_message.clone()).is_err() {
                    std::hint::spin_loop();
                }
                counter_prod.fetch_add(1, Ordering::Relaxed);
            }
        });
        
        b.iter(|| {
            let mut received = 0;
            while received < num_messages {
                if let Some(_msg) = queue.try_dequeue() {
                    received += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
        });
        
        producer.join().unwrap();
    });
    
    // Benchmark crossbeam channel
    group.bench_function("crossbeam_channel", |b| {
        let (tx, rx) = crossbeam::channel::bounded(65536);
        let counter = Arc::new(AtomicU64::new(0));
        
        // Spawn producer
        let tx_prod = tx.clone();
        let counter_prod = counter.clone();
        let producer = thread::spawn(move || {
            for _ in 0..num_messages {
                tx_prod.send(test_message.clone()).unwrap();
                counter_prod.fetch_add(1, Ordering::Relaxed);
            }
        });
        
        b.iter(|| {
            let mut received = 0;
            while received < num_messages {
                if let Ok(_msg) = rx.try_recv() {
                    received += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
        });
        
        producer.join().unwrap();
    });
    
    group.finish();
}

/// Benchmark message routing performance
fn benchmark_message_routing(c: &mut Criterion) {
    let mut group = c.benchmark_group("message_routing");
    
    // Test data
    let topic = "test/topic";
    let payload = Bytes::from("test payload");
    let num_subscribers = 100;
    let num_messages = 1000;
    
    group.bench_function("ultra_fast_routing", |b| {
        use blipmq::v2::routing::RoutingEngine;
        
        let routing = Arc::new(RoutingEngine::new());
        
        // Add subscribers
        for i in 0..num_subscribers {
            routing.subscribe(i, topic.to_string());
        }
        
        b.iter(|| {
            for _ in 0..num_messages {
                let subscribers = routing.get_subscribers(topic);
                black_box(subscribers);
            }
        });
    });
    
    group.finish();
}

/// Benchmark memory allocation patterns
fn benchmark_memory_allocation(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory_allocation");
    
    let test_data = b"test message data";
    
    // Benchmark stack allocation (small messages)
    group.bench_function("stack_allocation", |b| {
        b.iter(|| {
            let mut buffer = [0u8; 256];
            buffer[..test_data.len()].copy_from_slice(test_data);
            black_box(buffer);
        });
    });
    
    // Benchmark heap allocation
    group.bench_function("heap_allocation", |b| {
        b.iter(|| {
            let mut buffer = Vec::with_capacity(256);
            buffer.extend_from_slice(test_data);
            black_box(buffer);
        });
    });
    
    // Benchmark Bytes (zero-copy)
    group.bench_function("zero_copy_bytes", |b| {
        b.iter(|| {
            let bytes = Bytes::copy_from_slice(test_data);
            black_box(bytes);
        });
    });
    
    group.finish();
}

/// Benchmark concurrent message processing
fn benchmark_concurrent_processing(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_processing");
    
    let num_threads = num_cpus::get();
    let messages_per_thread = 1000;
    
    group.bench_function("multi_thread_processing", |b| {
        b.iter(|| {
            let queue = Arc::new(MpmcQueue::new(65536));
            let counter = Arc::new(AtomicU64::new(0));
            
            let mut handles = Vec::new();
            
            // Spawn producer threads
            for _ in 0..num_threads {
                let queue_prod = queue.clone();
                let counter_prod = counter.clone();
                let handle = thread::spawn(move || {
                    for i in 0..messages_per_thread {
                        let msg = Bytes::from(format!("message_{}", i));
                        while queue_prod.try_enqueue(msg).is_err() {
                            std::hint::spin_loop();
                        }
                        counter_prod.fetch_add(1, Ordering::Relaxed);
                    }
                });
                handles.push(handle);
            }
            
            // Spawn consumer threads
            for _ in 0..num_threads {
                let queue_cons = queue.clone();
                let handle = thread::spawn(move || {
                    let mut received = 0;
                    while received < messages_per_thread {
                        if let Some(_msg) = queue_cons.try_dequeue() {
                            received += 1;
                        } else {
                            std::hint::spin_loop();
                        }
                    }
                });
                handles.push(handle);
            }
            
            // Wait for all threads
            for handle in handles {
                handle.join().unwrap();
            }
            
            black_box(counter.load(Ordering::Relaxed));
        });
    });
    
    group.finish();
}

/// Benchmark end-to-end latency
fn benchmark_end_to_end_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("end_to_end_latency");
    
    group.bench_function("ultra_fast_pipeline", |b| {
        let mut frame_builder = FrameBuilder::new();
        let queue = Arc::new(MpmcQueue::new(1024));
        
        b.iter(|| {
            let start = Instant::now();
            
            // Simulate full pipeline: parse -> route -> deliver
            let topic = "test/topic";
            let payload = b"test message";
            let ttl = 60000u32;
            
            // 1. Parse message
            let frame = frame_builder.publish(topic.as_bytes(), payload, ttl);
            
            // 2. Route message
            let subscribers = vec![1, 2, 3, 4, 5]; // Simulate 5 subscribers
            
            // 3. Deliver to subscribers
            for _sub_id in subscribers {
                queue.try_enqueue(frame.clone()).ok();
            }
            
            // 4. Consume messages
            let mut delivered = 0;
            while delivered < 5 {
                if queue.try_dequeue().is_some() {
                    delivered += 1;
                }
            }
            
            let duration = start.elapsed();
            black_box(duration);
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    benchmark_protocol_parsing,
    benchmark_message_queues,
    benchmark_message_routing,
    benchmark_memory_allocation,
    benchmark_concurrent_processing,
    benchmark_end_to_end_latency
);

criterion_main!(benches);
