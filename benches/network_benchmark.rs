use blipmq::config::CONFIG;
use blipmq::core::command::{encode_command, new_pub, new_sub};
use blipmq::start_broker;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use log::{error, info};
use nats;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{sink, BufWriter};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio::sync::Barrier;
use tokio::sync::Mutex as TokioMutex;

const NUM_SUBSCRIBERS: usize = 25;
const NUM_MESSAGES: usize = 25000;
const MAX_BATCH: usize = 128;
const DEFAULT_TTL_MS: u64 = 30_000;

fn run_network_qos0_benchmark() -> (f64, f64) {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let addr = "127.0.0.1:7000";
        let topic = "bench_topic".to_string();
        let barrier = Arc::new(Barrier::new(NUM_SUBSCRIBERS + 1));

        info!("Starting BlipMQ server on {}", addr);
        let server_handle = tokio::spawn({
            let addr = addr.to_string();
            async move {
                let _ = start_broker(&addr, MAX_BATCH, DEFAULT_TTL_MS, NUM_MESSAGES).await;
            }
        });

        tokio::time::sleep(Duration::from_millis(300)).await;

        let mut sub_handles = Vec::with_capacity(NUM_SUBSCRIBERS);
        for i in 0..NUM_SUBSCRIBERS {
            let addr = addr.to_string();
            let topic = topic.clone();
            let b = barrier.clone();
            sub_handles.push(tokio::spawn(async move {
                let stream = TcpStream::connect(&addr).await.unwrap();
                stream.set_nodelay(true).expect("Disable Nagle failed");
                let (read_half, mut write_half) = stream.into_split();
                let cmd = new_sub(topic.clone());
                let encoded = encode_command(&cmd);
                let len = (encoded.len() as u32).to_be_bytes();

                write_half.write_all(&len).await.unwrap();
                write_half.write_all(&encoded).await.unwrap();

                let mut reader = BufReader::new(read_half);
                let mut dummy = String::new();
                reader.read_line(&mut dummy).await.unwrap(); // OK SUB

                b.wait().await; // wait for publisher

                let mut received = 0;
                while received < NUM_MESSAGES {
                    let mut len_buf = [0u8; 4];
                    if reader.read_exact(&mut len_buf).await.is_err() {
                        break;
                    }
                    let msg_len = u32::from_be_bytes(len_buf) as usize;
                    let mut buf = vec![0u8; msg_len];
                    if reader.read_exact(&mut buf).await.is_err() {
                        break;
                    }
                    received += 1;
                }
                assert_eq!(received, NUM_MESSAGES, "[Sub {}] Message loss!", i);
                received
            }));
        }

        barrier.wait().await; // publisher waits here

        let mut pub_stream = TcpStream::connect(addr).await.unwrap();
        pub_stream.set_nodelay(true).expect("Nagle off failed");

        let start = Instant::now();
        for i in 0..NUM_MESSAGES {
            let cmd = new_pub(topic.clone(), format!("msg-{}", i));
            let encoded = encode_command(&cmd);
            let len = (encoded.len() as u32).to_be_bytes();
            pub_stream.write_all(&len).await.unwrap();
            pub_stream.write_all(&encoded).await.unwrap();

            if i % 10_000 == 0 {
                info!("Published {}/{}", i, NUM_MESSAGES);
            }
        }
        pub_stream.flush().await.unwrap();
        let publish_duration = start.elapsed();

        for h in sub_handles {
            let _ = h.await.unwrap();
        }

        let total_duration = start.elapsed();
        server_handle.abort();
        let _ = server_handle.await;

        let total_fanout = NUM_MESSAGES * NUM_SUBSCRIBERS;
        let throughput = total_fanout as f64 / total_duration.as_secs_f64();
        let mean_latency_us = total_duration.as_secs_f64() / total_fanout as f64 * 1e6;

        info!(
            "BlipMQ QoS0 => pub_time={:?}, total_time={:?}, throughput={:.2} msgs/s, mean_latency={:.2}µs",
            publish_duration, total_duration, throughput, mean_latency_us
        );
        (throughput, mean_latency_us)
    })
}

fn run_nats_benchmark() -> (f64, f64) {
    let nc = nats::connect("127.0.0.1:4222").expect("Connect to NATS failed");

    let subs: Vec<_> = (0..NUM_SUBSCRIBERS)
        .map(|_| nc.subscribe("bench.nats").unwrap())
        .collect();

    let start = Instant::now();
    for i in 0..NUM_MESSAGES {
        nc.publish("bench.nats", format!("msg-{}", i)).unwrap();
        if i % 10_000 == 0 {
            info!("Published {}/{}", i, NUM_MESSAGES);
        }
    }
    nc.flush().unwrap();

    for (i, sub) in subs.iter().enumerate() {
        let mut received = 0;
        while received < NUM_MESSAGES {
            match sub.next_timeout(Duration::from_millis(500)) {
                Ok(_) => received += 1,
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => {
                    error!("NATS Sub {} error: {}", i, e);
                    break;
                }
            }
        }
        assert_eq!(received, NUM_MESSAGES, "NATS Sub {} dropped messages", i);
    }

    let total_duration = start.elapsed();
    let total_fanout = NUM_MESSAGES * NUM_SUBSCRIBERS;
    let throughput = total_fanout as f64 / total_duration.as_secs_f64();
    let mean_latency_us = total_duration.as_secs_f64() / total_fanout as f64 * 1e6;

    info!(
        "NATS => total_time={:?}, throughput={:.2} msgs/s, mean_latency={:.2}µs",
        total_duration, throughput, mean_latency_us
    );

    (throughput, mean_latency_us)
}

fn qos0_network_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("qos0_network");
    group.throughput(Throughput::Elements(
        (NUM_MESSAGES * NUM_SUBSCRIBERS) as u64,
    ));
    group.sample_size(10);
    group.measurement_time(Duration::new(25, 0));

    // Warm-up (optional but recommended)
    let _ = run_network_qos0_benchmark();

    let mut lat_blip = 0.0;
    group.bench_function("blipmq_qos0_tcp", |b| {
        b.iter(|| {
            let (tp, lat) = run_network_qos0_benchmark();
            lat_blip = lat;
            tp
        })
    });

    let mut lat_nats = 0.0;
    group.bench_function("nats_tcp", |b| {
        b.iter(|| {
            let (tp, lat) = run_nats_benchmark();
            lat_nats = lat;
            tp
        })
    });

    group.finish();

    println!("\n=== Latency Summary ===");
    println!("BlipMQ mean latency: {:.2} µs", lat_blip);
    println!("NATS mean latency: {:.2} µs", lat_nats);
}

criterion_group!(benches, qos0_network_benchmark);
criterion_main!(benches);
