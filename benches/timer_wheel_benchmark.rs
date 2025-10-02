use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use blipmq::core::message::WireMessage;
use blipmq::core::timer_wheel::{TimerManager, TimerWheel};
use bytes::Bytes;

fn create_test_message(expire_at: u64) -> Arc<WireMessage> {
    Arc::new(WireMessage {
        frame: Bytes::from(vec![0u8; 1024]), // 1KB payload
        expire_at,
    })
}

fn create_messages_batch(count: usize, base_ttl: u64) -> Vec<Arc<WireMessage>> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    (0..count)
        .map(|i| {
            let ttl_offset = (i % 1000) as u64 * 100; // Spread TTLs over 100 seconds
            create_test_message(now + base_ttl + ttl_offset)
        })
        .collect()
}

fn bench_timer_wheel_insertion(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer_wheel_insertion");
    
    for &size in [1000, 10_000, 100_000].iter() {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("insert", size), &size, |b, &size| {
            let wheel = TimerWheel::new();
            let messages = create_messages_batch(size, 60_000); // 1 minute base TTL
            
            b.iter(|| {
                for msg in &messages {
                    black_box(wheel.insert(msg.clone()).unwrap());
                }
                // Clear for next iteration
                wheel.clear();
            });
        });
    }
    
    group.finish();
}

fn bench_timer_wheel_basic(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer_wheel_basic");
    
    group.bench_function("insert_1000", |b| {
        let wheel = TimerWheel::new();
        let messages = create_messages_batch(1000, 60_000);
        
        b.iter(|| {
            for msg in &messages {
                black_box(wheel.insert(msg.clone()).unwrap());
            }
            wheel.clear();
        });
    });
    
    group.bench_function("timer_manager_basic", |b| {
        let manager = TimerManager::new();
        let messages = create_messages_batch(100, 60_000);
        
        b.iter(|| {
            for msg in &messages {
                black_box(manager.schedule_expiration(msg.clone()).unwrap());
            }
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    bench_timer_wheel_insertion,
    bench_timer_wheel_basic
);

criterion_main!(benches);