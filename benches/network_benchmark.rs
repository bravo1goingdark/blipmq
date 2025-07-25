use blipmq::start_broker;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use nats;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;

const NUM_SUBSCRIBERS: usize = 250;
const NUM_MESSAGES: usize = 100000;
const MAX_BATCH: usize = 64;

fn run_network_qos0_benchmark() -> (f64, f64) {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let addr = "127.0.0.1:7000";
        let server_handle = tokio::spawn(async move {
            let _ = start_broker(addr, MAX_BATCH).await;
        });

        // Give the server a moment to start
        tokio::time::sleep(Duration::from_millis(200)).await;

        let topic = "bench_topic".to_string();

        let mut sub_handles = Vec::with_capacity(NUM_SUBSCRIBERS);
        for _ in 0..NUM_SUBSCRIBERS {
            let addr = addr.to_string();
            let topic = topic.clone();
            sub_handles.push(tokio::spawn(async move {
                let stream = TcpStream::connect(addr).await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                write_half
                    .write_all(format!("SUB {}\n", topic).as_bytes())
                    .await
                    .unwrap();
                let mut reader = BufReader::new(read_half);
                // Read OK SUB line
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();

                let mut received = 0usize;
                while received < NUM_MESSAGES {
                    let mut len_buf = [0u8; 4];
                    if reader.read_exact(&mut len_buf).await.is_err() {
                        break;
                    }
                    let msg_len = u32::from_be_bytes(len_buf) as usize;
                    let mut msg_buf = vec![0u8; msg_len];
                    if reader.read_exact(&mut msg_buf).await.is_err() {
                        break;
                    }
                    received += 1;
                }
                received
            }));
        }

        // Wait a moment for subscribers to establish
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut pub_stream = TcpStream::connect(addr).await.unwrap();

        let start = Instant::now();
        for i in 0..NUM_MESSAGES {
            let cmd = format!("PUB {} msg-{}\n", topic, i);
            pub_stream.write_all(cmd.as_bytes()).await.unwrap();
        }
        pub_stream.flush().await.unwrap();
        let publish_duration = start.elapsed();

        for handle in sub_handles {
            let _ = handle.await.unwrap();
        }
        let total_duration = start.elapsed();

        // Shut down server
        server_handle.abort();
        let _ = server_handle.await;

        let total_fanout = NUM_MESSAGES * NUM_SUBSCRIBERS;
        let throughput = total_fanout as f64 / total_duration.as_secs_f64();
        let mean_latency_us = total_duration.as_secs_f64() / total_fanout as f64 * 1e6;

        println!(
            "Network QoS0 => pub_time={:?}, total_time={:?}, throughput={:.2} msgs/s, mean_latency={:.2}µs",
            publish_duration, total_duration, throughput, mean_latency_us
        );
        (throughput, mean_latency_us)
    })
}

fn run_nats_benchmark() -> (f64, f64) {
    // Expect a running NATS server on localhost:4222
    let nc = nats::connect("127.0.0.1:4222").expect("failed to connect to NATS at localhost:4222");

    let subs: Vec<_> = (0..NUM_SUBSCRIBERS)
        .map(|_| nc.subscribe("bench.nats").unwrap())
        .collect();

    let start = Instant::now();
    for i in 0..NUM_MESSAGES {
        nc.publish("bench.nats", format!("msg-{}", i)).unwrap();
    }
    nc.flush().unwrap();

    for sub in &subs {
        let mut received = 0;
        while received < NUM_MESSAGES {
            match sub.next_timeout(Duration::from_millis(500)) {
                Ok(_m) => received += 1,
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
    let (tp_blip, lat_blip) = run_network_qos0_benchmark();
    group.bench_function("blipmq_qos0_network", |b| b.iter(|| tp_blip));

    let (tp_nats, lat_nats) = run_nats_benchmark();
    group.bench_function("nats", |b| b.iter(|| tp_nats));
    group.finish();
    println!("\n=== Latency Summary ===");
    println!("BlipMQ QoS0 network mean latency: {:.2} µs", lat_blip);
    println!("NATS mean latency: {:.2} µs", lat_nats);
}

criterion_group!(benches, qos0_network_benchmark);
criterion_main!(benches);
