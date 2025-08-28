use blipmq::config::CONFIG;
use blipmq::core::command::{encode_command, new_pub, new_sub};
use blipmq::core::message::{decode_frame, ServerFrame};
use blipmq::start_broker;

use anyhow::Context;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    runtime::Runtime,
    sync::Barrier,
};
use tracing::{error, info, warn};

const NUM_SUBSCRIBERS: usize = 48;
const NUM_MESSAGES: usize = 48000;

async fn run_network_qos0_benchmark_inner() -> anyhow::Result<(f64, f64)> {
    let addr = CONFIG.server.bind_addr.as_str();
    let topic = "bench_topic".to_string();
    let barrier = Arc::new(Barrier::new(NUM_SUBSCRIBERS + 1));

    let server_handle = tokio::spawn(async move {
        if let Err(e) = start_broker().await {
            error!("Broker failed to start or encountered an error: {:?}", e);
        }
    });

    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut sub_handles = Vec::new();
    for i in 0..NUM_SUBSCRIBERS {
        let addr = addr.to_string();
        let topic = topic.clone();
        let barrier = barrier.clone();

        sub_handles.push(tokio::spawn(async move {
            let stream = TcpStream::connect(&addr)
                .await
                .with_context(|| format!("Subscriber {} failed to connect to {}", i, addr))?;
            stream.set_nodelay(true).context("Failed to set no_delay for subscriber stream")?;
            let (read_half, mut write_half) = stream.into_split();

            let cmd = new_sub(topic.clone());
            let enc = encode_command(&cmd);
            write_half.write_all(&(enc.len() as u32).to_be_bytes()).await
                .context("Subscriber failed to write length prefix")?;
            write_half.write_all(&enc).await
                .context("Subscriber failed to write command")?;
            write_half.flush().await
                .context("Subscriber failed to flush command")?;

            let mut reader = BufReader::new(read_half);

            // Read SubAck
            let mut len_buf = [0u8; 4];
            reader.read_exact(&mut len_buf).await
                .context("Subscriber failed to read SubAck length")?;
            let frame_len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; frame_len];
            reader.read_exact(&mut buf).await
                .context("Subscriber failed to read SubAck payload")?;

            match decode_frame(&buf) {
                Ok(ServerFrame::SubAck(_)) => {},
                Ok(_) => anyhow::bail!("Subscriber {} received invalid SubAck frame", i),
                Err(e) => anyhow::bail!("Subscriber {} failed to decode SubAck: {}", i, e),
            }

            barrier.wait().await;

            let mut received = 0;
            while received < NUM_MESSAGES {
                let mut len_buf = [0u8; 4];
                if reader.read_exact(&mut len_buf).await.is_err() {
                    warn!("Subscriber {} disconnected prematurely or failed to read message length.", i);
                    break;
                }
                let msg_len = u32::from_be_bytes(len_buf) as usize;
                let mut msg_buf = vec![0u8; msg_len];
                if reader.read_exact(&mut msg_buf).await.is_err() {
                    warn!("Subscriber {} disconnected prematurely or failed to read message payload.", i);
                    break;
                }
                received += 1;
            }

            anyhow::ensure!(received == NUM_MESSAGES, "Subscriber {} message loss: expected {}, got {}", i, NUM_MESSAGES, received);
            Ok(received)
        }));
    }

    barrier.wait().await;

    let mut pub_stream = TcpStream::connect(addr)
        .await
        .context("Publisher failed to connect to broker")?;
    pub_stream.set_nodelay(true).context("Failed to set no_delay for publisher stream")?;

    let start = Instant::now();
    for i in 0..NUM_MESSAGES {
        let cmd = new_pub(topic.clone(), format!("msg-{}", i));
        let enc = encode_command(&cmd);
        pub_stream.write_all(&(enc.len() as u32).to_be_bytes()).await
            .context("Publisher failed to write length prefix")?;
        pub_stream.write_all(&enc).await
            .context("Publisher failed to write command")?;
    }
    pub_stream.flush().await
        .context("Publisher failed to flush commands")?;
    let publish_duration = start.elapsed();

    for h in sub_handles {
        h.await.context("Subscriber task failed")??;
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

    Ok((throughput, mean_latency_us))
}

fn run_network_qos0_benchmark() -> (f64, f64) {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        match run_network_qos0_benchmark_inner().await {
            Ok(metrics) => metrics,
            Err(e) => {
                error!("BlipMQ benchmark failed: {:?}", e);
                (0.0, 0.0) // Return zero metrics on failure
            }
        }
    })
}

async fn run_nats_benchmark_inner() -> anyhow::Result<(f64, f64)> {
    let nc = nats::connect("127.0.0.1:4222")
        .context("Failed to connect to NATS. Is NATS server running on 127.0.0.1:4222?")?;

    let mut subs = Vec::new();
    for i in 0..NUM_SUBSCRIBERS {
        subs.push(nc.subscribe("bench.nats")
            .with_context(|| format!("NATS Sub {} failed to subscribe", i))?);
    }

    let start = Instant::now();
    for i in 0..NUM_MESSAGES {
        nc.publish("bench.nats", format!("msg-{}", i))
            .with_context(|| format!("NATS Pub failed to publish message {}", i))?;
    }
    nc.flush()
        .context("NATS Pub failed to flush messages")?;

    for (i, sub) in subs.iter().enumerate() {
        let mut received = 0;
        while received < NUM_MESSAGES {
            match sub.next_timeout(Duration::from_millis(60)) {
                Ok(_) => received += 1,
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    warn!("NATS Sub {} timed out waiting for messages. Received {} of {}", i, received, NUM_MESSAGES);
                    break; // Exit loop if timeout occurs, report what was received
                }
                Err(e) => {
                    error!("NATS Sub {} error: {:?}", i, e);
                    anyhow::bail!("NATS Sub {} encountered an error: {:?}", i, e);
                }
            }
        }
        anyhow::ensure!(received == NUM_MESSAGES, "NATS Sub {} message loss: expected {}, got {}", i, NUM_MESSAGES, received);
    }

    let total_duration = start.elapsed();
    let total_fanout = NUM_MESSAGES * NUM_SUBSCRIBERS;
    let throughput = total_fanout as f64 / total_duration.as_secs_f64();
    let mean_latency_us = total_duration.as_secs_f64() / total_fanout as f64 * 1e6;

    info!(
        "📦 NATS → total_time={:?}, throughput={:.2} msg/s, mean_latency={:.2}µs",
        total_duration, throughput, mean_latency_us
    );

    Ok((throughput, mean_latency_us))
}

fn run_nats_benchmark() -> (f64, f64) {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        match run_nats_benchmark_inner().await {
            Ok(metrics) => metrics,
            Err(e) => {
                error!("NATS benchmark failed: {:?}", e);
                (0.0, 0.0) // Return zero metrics on failure
            }
        }
    })
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
        b.iter_custom(|iters| {
            let mut total_throughput = 0.0;
            for _ in 0..iters {
                let (tp, lat) = run_network_qos0_benchmark();
                latency_blip = lat; // Capture last latency
                total_throughput += tp;
            }
            total_throughput / iters as f64
        })
    });

    let mut latency_nats = 0.0;
    group.bench_function("nats_tcp", |b| {
        b.iter_custom(|iters| {
            let mut total_throughput = 0.0;
            for _ in 0..iters {
                let (tp, lat) = run_nats_benchmark();
                latency_nats = lat; // Capture last latency
                total_throughput += tp;
            }
            total_throughput / iters as f64
        })
    });

    group.finish();

    println!("\n=== Latency Summary ===");
    println!("BlipMQ mean latency: {:.2} µs", latency_blip);
    println!("NATS mean latency: {:.2} µs", latency_nats);
}

criterion_group!(benches, qos0_network_benchmark);
criterion_main!(benches);
