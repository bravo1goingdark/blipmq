use blipmq::core::delivery_mode::DeliveryMode;
use blipmq::core::message::{new_message, Message};
use blipmq::core::publisher::{Publisher, PublisherConfig};
use blipmq::core::subscriber::{Subscriber, SubscriberId};
use blipmq::core::topics::registry::TopicRegistry;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::Receiver;

const NUM_SUBSCRIBERS: usize = 100;
const NUM_MESSAGES: usize = 100_000;
const RECV_TIMEOUT_MS: u64 = 1000;

fn run_qos0_delivery_test() -> (f64, f64) {
    let rt = Runtime::new().expect("Failed to create Tokio runtime");

    rt.block_on(async {
        let registry = Arc::new(TopicRegistry::new());
        let publisher = Publisher::new(
            Arc::clone(&registry),
            PublisherConfig {
                delivery_mode: DeliveryMode::Ordered, // QoS0 uses Ordered
            },
        );

        let topic_name = "perf_qos0".to_string();
        let topic = registry.create_or_get_topic(&topic_name);

        // Create subscribers and capture their receivers
        let mut receivers: Vec<Receiver<Arc<Message>>> = Vec::with_capacity(NUM_SUBSCRIBERS);
        for i in 0..NUM_SUBSCRIBERS {
            let subscriber_id = SubscriberId::from(format!("sub-{i}"));
            let (subscriber, rx) = Subscriber::new(subscriber_id);
            receivers.push(rx);
            topic.subscribe(subscriber).await;
        }

        // Publish all messages
        let start = Instant::now();
        for i in 0..NUM_MESSAGES {
            let msg = Arc::new(new_message(format!("msg-{i}")));
            publisher.publish(&topic_name, msg).await;
        }
        let publish_duration = start.elapsed();

        // Receive from each subscriber
        for rx in &mut receivers {
            let mut received = 0;
            while received < NUM_MESSAGES {
                match tokio::time::timeout(
                    Duration::from_millis(RECV_TIMEOUT_MS),
                    rx.recv(),
                )
                    .await
                {
                    Ok(Some(_)) => received += 1,
                    _ => break,
                }
            }
        }
        let total_duration = start.elapsed();

        let total_fanout = NUM_MESSAGES * NUM_SUBSCRIBERS;
        let throughput = total_fanout as f64 / total_duration.as_secs_f64();
        let mean_latency_us = total_duration.as_secs_f64() / total_fanout as f64 * 1e6;

        println!(
            "QoS0 Ordered Delivery => pub_time={:?}, total_time={:?}, fanout={}, throughput={:.2} msgs/s, mean_latency={:.2}µs",
            publish_duration,
            total_duration,
            total_fanout,
            throughput,
            mean_latency_us
        );

        (throughput, mean_latency_us)
    })
}

fn benchmark_qos0(c: &mut Criterion) {
    let mut group = c.benchmark_group("qos0_delivery");
    group.throughput(Throughput::Elements(
        (NUM_MESSAGES * NUM_SUBSCRIBERS) as u64,
    ));

    let (throughput, mean_latency) = run_qos0_delivery_test();
    group.bench_function("QoS0_Throughput", |b| b.iter(|| throughput));

    group.finish();

    println!("\n=== Latency Summary ===");
    println!("QoS0 mean latency: {:.2} µs", mean_latency);
}

criterion_group!(benches, benchmark_qos0);
criterion_main!(benches);
