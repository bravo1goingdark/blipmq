use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::sync::Arc;
use std::time::{Duration, Instant};

use blipmq::core::delivery_mode::DeliveryMode;
use blipmq::core::message::{new_message, Message};
use blipmq::core::publisher::{Publisher, PublisherConfig};
use blipmq::core::subscriber::{Subscriber, SubscriberId};
use blipmq::core::topics::registry::TopicRegistry;
use crossbeam_channel::Receiver;
use tokio::runtime::Runtime;

const NUM_MESSAGES: usize = 100_000;
const NUM_SUBSCRIBERS: usize = 10;

fn run_blipmq_ordered() -> (f64, f64) {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let registry = Arc::new(TopicRegistry::new());
        let publisher = Publisher::new(
            Arc::clone(&registry),
            PublisherConfig {
                delivery_mode: DeliveryMode::Ordered,
            },
        );
        let topic_name = "perf_topic".to_string();
        let topic = registry.create_or_get_topic(&topic_name);

        let mut receivers: Vec<Receiver<Arc<Message>>> = Vec::with_capacity(NUM_SUBSCRIBERS);
        for i in 0..NUM_SUBSCRIBERS {
            let subscriber_id = SubscriberId::from(format!("sub-{}", i));
            let subscriber = Subscriber::new(subscriber_id);
            receivers.push(subscriber.receiver().clone());
            topic.subscribe(subscriber).await;
        }

        let start = Instant::now();
        for i in 0..NUM_MESSAGES {
            let msg = Arc::new(new_message(format!("msg-{}", i)));
            publisher.publish(&topic_name, msg).await;
        }
        let publish_duration = start.elapsed();

        for rx in receivers {
            let mut received = 0;
            while received < NUM_MESSAGES {
                if let Ok(_) = rx.recv_timeout(Duration::from_millis(500)) {
                    received += 1;
                } else {
                    break;
                }
            }
        }

        let total_duration = start.elapsed();
        let total_fanout = NUM_MESSAGES * NUM_SUBSCRIBERS;
        let throughput = total_fanout as f64 / total_duration.as_secs_f64();
        let mean_latency_us = total_duration.as_secs_f64() / total_fanout as f64 * 1e6;

        println!(
            "[BlipMQ] pub_time={:?}, total_time={:?}, throughput={:.2} msgs/s, mean_latency={:.2}µs",
            publish_duration, total_duration, throughput, mean_latency_us
        );

        (throughput, mean_latency_us)
    })
}

// ========== NATS ==========

fn run_nats() -> (f64, f64) {
    let nc = nats::connect("127.0.0.1").expect("NATS server not running");
    let subs: Vec<_> = (0..NUM_SUBSCRIBERS)
        .map(|_| nc.subscribe("test.nats").unwrap())
        .collect();

    let start = Instant::now();
    for i in 0..NUM_MESSAGES {
        nc.publish("test.nats", format!("msg-{}", i)).unwrap();
    }

    for sub in &subs {
        let mut received = 0;
        while received < NUM_MESSAGES {
            match sub.next_timeout(Duration::from_millis(500)) {
                Ok(_msg) => received += 1,
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => panic!("NATS receive error: {}", e),
            }
        }
    }

    let total_duration = start.elapsed();
    let total_fanout = NUM_MESSAGES * NUM_SUBSCRIBERS;
    let throughput = total_fanout as f64 / total_duration.as_secs_f64();
    let mean_latency_us = total_duration.as_secs_f64() / total_fanout as f64 * 1e6;

    println!(
        "[NATS] total_time={:?}, throughput={:.2} msgs/s, mean_latency={:.2}µs",
        total_duration, throughput, mean_latency_us
    );

    (throughput, mean_latency_us)
}

// ========== Criterion Benchmark Group ==========

fn broker_comparison_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("broker_comparison");
    group.throughput(Throughput::Elements(
        (NUM_MESSAGES * NUM_SUBSCRIBERS) as u64,
    ));

    let (tp_blip, lat_blip) = run_blipmq_ordered();
    group.bench_function("blipmq_ordered", |b| b.iter(|| tp_blip));

    let (tp_nats, lat_nats) = run_nats();
    group.bench_function("nats", |b| b.iter(|| tp_nats));

    group.finish();

    println!("\n=== Mean Latency Summary ===");
    println!("BlipMQ Ordered: {:.2} µs", lat_blip);
    println!("NATS:           {:.2} µs", lat_nats);
}

criterion_group!(benches, broker_comparison_benchmark);
criterion_main!(benches);
