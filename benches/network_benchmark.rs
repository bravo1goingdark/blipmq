use blipmq::config::CONFIG;
use blipmq::core::command::{encode_command, new_pub, new_sub};
use blipmq::core::message::{decode_frame, ServerFrame};
use blipmq::start_broker;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use log::{error, info};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    runtime::Runtime,
    sync::Barrier,
};

const NUM_SUBSCRIBERS: usize = 48;
const NUM_MESSAGES: usize = 48000;

fn run_network_qos0_benchmark() -> (f64, f64) {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let addr = CONFIG.server.bind_addr.as_str();
        let topic = "bench_topic".to_string();
        let barrier = Arc::new(Barrier::new(NUM_SUBSCRIBERS + 1));

        let server_handle = tokio::spawn(async move {
            let _ = start_broker().await;
        });

        tokio::time::sleep(Duration::from_millis(300)).await;

        let mut sub_handles = Vec::new();
        for i in 0..NUM_SUBSCRIBERS {
            let addr = addr.to_string();
            let topic = topic.clone();
            let barrier = barrier.clone();

            sub_handles.push(tokio::spawn(async move {
                let stream = TcpStream::connect(&addr).await.unwrap();
                stream.set_nodelay(true).unwrap();
                let (read_half, mut write_half) = stream.into_split();

                let cmd = new_sub(topic.clone());
                let enc = encode_command(&cmd);
                write_half.write_all(&(enc.len() as u32).to_be_bytes()).await.unwrap();
                write_half.write_all(&enc).await.unwrap();
                write_half.flush().await.unwrap();

                let mut reader = BufReader::new(read_half);

                // Read SubAck
                let mut len_buf = [0u8; 4];
                reader.read_exact(&mut len_buf).await.unwrap();
                let frame_len = u32::from_be_bytes(len_buf) as usize;
                let mut buf = vec![0u8; frame_len];
                reader.read_exact(&mut buf).await.unwrap();

                match decode_frame(&buf) {
                    Ok(ServerFrame::SubAck(_)) => {}
                    Ok(_) => panic!("[Sub {}] Invalid SubAck frame", i),
                    Err(e) => panic!("[Sub {}] Decode SubAck failed: {}", i, e),
                }

                barrier.wait().await;

                let mut received = 0;
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

                assert_eq!(received, NUM_MESSAGES, "[Sub {}] message loss", i);
                received
            }));
        }

        barrier.wait().await;

        let mut pub_stream = TcpStream::connect(addr).await.unwrap();
        pub_stream.set_nodelay(true).unwrap();

        let start = Instant::now();
        for i in 0..NUM_MESSAGES {
            let cmd = new_pub(topic.clone(), format!("msg-{}", i));
            let enc = encode_command(&cmd);
            pub_stream.write_all(&(enc.len() as u32).to_be_bytes()).await.unwrap();
            pub_stream.write_all(&enc).await.unwrap();
        }
        pub_stream.flush().await.unwrap();
        let publish_duration = start.elapsed();

        for h in sub_handles {
            h.await.unwrap();
        }

        server_handle.abort();
        let _ = server_handle.await;

        let total_duration = start.elapsed();
        let total_fanout = NUM_MESSAGES * NUM_SUBSCRIBERS;
        let throughput = total_fanout as f64 / total_duration.as_secs_f64();
        let mean_latency_us = total_duration.as_secs_f64() / total_fanout as f64 * 1e6;

        info!(
            "✅ BlipMQ → pub_time={:?}, total_time={:?}, throughput={:.2} msg/s, mean_latency={:.2}µs",
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
    }
    nc.flush().unwrap();

    for (i, sub) in subs.iter().enumerate() {
        let mut received = 0;
        while received < NUM_MESSAGES {
            match sub.next_timeout(Duration::from_millis(60)) {
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
        "📦 NATS → total_time={:?}, throughput={:.2} msg/s, mean_latency={:.2}µs",
        total_duration, throughput, mean_latency_us
    );

    (throughput, mean_latency_us)
}

fn qos0_network_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("qos0_network_tcp");
    group.throughput(Throughput::Elements(
        (NUM_MESSAGES * NUM_SUBSCRIBERS) as u64,
    ));
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(110));

    let mut latency_blip = 0.0;
    group.bench_function("blipmq_qos0_tcp", |b| {
        b.iter(|| {
            let (tp, lat) = run_network_qos0_benchmark();
            latency_blip = lat;
            tp
        })
    });

    let mut latency_nats = 0.0;
    group.bench_function("nats_tcp", |b| {
        b.iter(|| {
            let (tp, lat) = run_nats_benchmark();
            latency_nats = lat;
            tp
        })
    });

    group.finish();

    println!("\n=== Latency Summary ===");
    println!("BlipMQ mean latency: {:.2} µs", latency_blip);
    println!("NATS mean latency: {:.2} µs", latency_nats);
}

criterion_group!(benches, qos0_network_benchmark);
criterion_main!(benches);
