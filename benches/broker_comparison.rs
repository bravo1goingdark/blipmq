use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use blipmq::core::delivery_mode::DeliveryMode;
use blipmq::core::message::new_message;
use blipmq::core::publisher::{Publisher, PublisherConfig};
use blipmq::core::subscriber::{Subscriber, SubscriberId};
use blipmq::core::topics::registry::TopicRegistry;
use hdrhistogram::Histogram;
use tokio::runtime::Runtime;

const NUM_MESSAGES: usize = 100000;
const NUM_SUBSCRIBERS: usize = 25;

// ================== BlipMQ Benchmark ===================

fn run_blipmq_ordered() -> (f64, f64, Histogram<u64>) {
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

        let mut receivers = Vec::with_capacity(NUM_SUBSCRIBERS);
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

        let histogram = Arc::new(Mutex::new(Histogram::<u64>::new_with_bounds(1, 10_000_000, 5).unwrap()));
        let mut handles = vec![];
        let total_received = Arc::new(Mutex::new(0u64));

        for rx in receivers {
            let hist = Arc::clone(&histogram);
            let count = Arc::clone(&total_received);
            let handle = thread::spawn(move || {
                let mut received = 0;
                while received < NUM_MESSAGES {
                    let t0 = Instant::now();
                    if let Ok(_) = rx.recv_timeout(Duration::from_millis(500)) {
                        let latency = t0.elapsed().as_micros() as u64;
                        hist.lock().unwrap().record(latency).unwrap();
                        received += 1;
                        *count.lock().unwrap() += 1;
                    }
                }
            });
            handles.push(handle);
        }

        for h in handles {
            h.join().unwrap();
        }

        let total_duration = start.elapsed();
        let received = *total_received.lock().unwrap();
        let throughput = received as f64 / total_duration.as_secs_f64();
        let mean_latency_us = total_duration.as_secs_f64() / received as f64 * 1e6;

        println!(
            "[BlipMQ] pub_time={:?}, total_time={:?}, throughput={:.2} msgs/s, mean_latency={:.2}µs",
            publish_duration, total_duration, throughput, mean_latency_us
        );

        (throughput, mean_latency_us, Arc::try_unwrap(histogram).unwrap().into_inner().unwrap())
    })
}

// ================== NATS Benchmark ===================

fn run_nats() -> (f64, f64, Histogram<u64>) {
    let nc = nats::connect("127.0.0.1").expect("NATS server not running");
    let subs: Vec<_> = (0..NUM_SUBSCRIBERS)
        .map(|_| nc.subscribe("test.nats").unwrap())
        .collect();

    let start = Instant::now();
    for i in 0..NUM_MESSAGES {
        nc.publish("test.nats", format!("msg-{}", i)).unwrap();
    }
    nc.flush().unwrap();

    let histogram = Arc::new(Mutex::new(
        Histogram::<u64>::new_with_bounds(1, 10_000_000, 5).unwrap(),
    ));
    let total_received = Arc::new(Mutex::new(0u64));
    let mut handles = vec![];

    for sub in subs {
        let hist = Arc::clone(&histogram);
        let count = Arc::clone(&total_received);
        let handle = thread::spawn(move || {
            let mut received = 0;
            while received < NUM_MESSAGES {
                let t0 = Instant::now();
                match sub.next_timeout(Duration::from_millis(500)) {
                    Ok(_) => {
                        let latency = t0.elapsed().as_micros() as u64;
                        hist.lock().unwrap().record(latency).unwrap();
                        received += 1;
                        *count.lock().unwrap() += 1;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                    Err(e) => panic!("NATS receive error: {}", e),
                }
            }
        });
        handles.push(handle);
    }

    for h in handles {
        h.join().unwrap();
    }

    let total_duration = start.elapsed();
    let received = *total_received.lock().unwrap();
    let throughput = received as f64 / total_duration.as_secs_f64();
    let mean_latency_us = total_duration.as_secs_f64() / received as f64 * 1e6;

    println!(
        "[NATS] total_time={:?}, throughput={:.2} msgs/s, mean_latency={:.2}µs",
        total_duration, throughput, mean_latency_us
    );

    (
        throughput,
        mean_latency_us,
        Arc::try_unwrap(histogram).unwrap().into_inner().unwrap(),
    )
}

// ================== Criterion Group ===================

fn broker_comparison_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("broker_comparison");
    group.throughput(Throughput::Elements(
        (NUM_MESSAGES * NUM_SUBSCRIBERS) as u64,
    ));

    let (tp_blip, lat_blip, hist_blip) = run_blipmq_ordered();
    group.bench_function("blipmq_ordered", |b| b.iter(|| tp_blip));

    let (tp_nats, lat_nats, hist_nats) = run_nats();
    group.bench_function("nats", |b| b.iter(|| tp_nats));

    group.finish();

    println!("\n=== Mean Latency Summary ===");
    println!("BlipMQ Ordered: {:.2} µs", lat_blip);
    println!("NATS:           {:.2} µs", lat_nats);

    println!("\n=== BlipMQ Percentiles (µs) ===");
    println!("p50: {:>6.2}", hist_blip.value_at_quantile(0.50));
    println!("p95: {:>6.2}", hist_blip.value_at_quantile(0.95));
    println!("p99: {:>6.2}", hist_blip.value_at_quantile(0.99));
    println!("max: {:>6.2}", hist_blip.max());

    println!("\n=== NATS Percentiles (µs) ===");
    println!("p50: {:>6.2}", hist_nats.value_at_quantile(0.50));
    println!("p95: {:>6.2}", hist_nats.value_at_quantile(0.95));
    println!("p99: {:>6.2}", hist_nats.value_at_quantile(0.99));
    println!("max: {:>6.2}", hist_nats.max());
}

criterion_group!(benches, broker_comparison_benchmark);
criterion_main!(benches);
