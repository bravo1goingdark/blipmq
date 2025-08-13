use blipmq::config::CONFIG;
use blipmq::core::command::{encode_command_into, new_pub, new_sub};
use blipmq::core::message::{decode_frame, decode_message, ServerFrame};
use blipmq::start_broker;

use bytes::BytesMut;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use hdrhistogram::Histogram;
use log::{error, info};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    runtime::Runtime,
    sync::Barrier,
};

const NUM_SUBSCRIBERS: usize = 5;
const NUM_MESSAGES: usize = 5000;
const PUB_BATCH: usize = 256;

// helper: epoch millis
#[inline]
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn run_network_qos0_benchmark() -> (f64, f64) {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let addr = CONFIG.server.bind_addr.as_str();
        let topic = "bench_topic".to_string();
        let barrier = Arc::new(Barrier::new(NUM_SUBSCRIBERS + 1));

        // Start server
        let server_handle = tokio::spawn(async move {
            let _ = start_broker().await;
        });

        tokio::time::sleep(Duration::from_millis(300)).await;

        // Spawn subscribers
        let mut sub_handles = Vec::with_capacity(NUM_SUBSCRIBERS);
        for i in 0..NUM_SUBSCRIBERS {
            let addr = addr.to_string();
            let topic = topic.clone();
            let barrier = barrier.clone();

            sub_handles.push(tokio::spawn(async move {
                let stream = TcpStream::connect(&addr).await.unwrap();
                stream.set_nodelay(true).unwrap();
                let (read_half, mut write_half) = stream.into_split();

                // SUB
                let cmd = new_sub(topic.clone());
                let mut frame = BytesMut::with_capacity(128);
                encode_command_into(&cmd, &mut frame);
                write_half
                    .write_all(&(frame.len() as u32).to_be_bytes())
                    .await
                    .unwrap();
                write_half.write_all(&frame).await.unwrap();
                write_half.flush().await.unwrap();

                let mut reader = BufReader::new(read_half);

                // SubAck
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

                // Histogram: 1us..120s, 3 sig figs
                let mut hist = Histogram::<u64>::new_with_bounds(1, 120_000_000, 3).unwrap();

                // Reusable msg buffer
                let mut msg_buf: Vec<u8> = Vec::with_capacity(4096);

                let mut received = 0usize;
                while received < NUM_MESSAGES {
                    // length prefix
                    let mut lb = [0u8; 4];
                    if reader.read_exact(&mut lb).await.is_err() {
                        break;
                    }
                    let msg_len = u32::from_be_bytes(lb) as usize;

                    // read payload into reusable buffer
                    msg_buf.resize(msg_len, 0);
                    if reader.read_exact(&mut msg_buf).await.is_err() {
                        break;
                    }

                    // decode Message (flatbuffer, no type tag)
                    if let Ok(m) = decode_message(&msg_buf) {
                        // Broker stamps timestamp in **milliseconds**
                        let now = now_ms();
                        let latency_us = (now.saturating_sub(m.timestamp)) * 1000;
                        let _ = hist.record(latency_us);
                    }
                    received += 1;
                }

                assert_eq!(received, NUM_MESSAGES, "[Sub {}] message loss", i);
                hist
            }));
        }

        // Wait until all subs are ready
        barrier.wait().await;

        // Publisher (batched frames)
        let mut pub_stream = TcpStream::connect(addr).await.unwrap();
        pub_stream.set_nodelay(true).unwrap();

        let mut send_buf = BytesMut::with_capacity(PUB_BATCH * 128);
        let mut tmp = BytesMut::with_capacity(256);

        let start = Instant::now();
        for i in 0..NUM_MESSAGES {
            tmp.clear();
            encode_command_into(&new_pub(topic.clone(), format!("msg-{}", i)), &mut tmp);
            let len = tmp.len() as u32;
            send_buf.extend_from_slice(&len.to_be_bytes());
            send_buf.extend_from_slice(&tmp);

            if (i + 1) % PUB_BATCH == 0 {
                pub_stream.write_all(&send_buf).await.unwrap();
                send_buf.clear();
            }
        }
        if !send_buf.is_empty() {
            pub_stream.write_all(&send_buf).await.unwrap();
        }
        pub_stream.flush().await.unwrap();
        let publish_duration = start.elapsed();

        // Collect histograms and merge
        let mut merged =
            Histogram::<u64>::new_with_bounds(1, 120_000_000, 3).expect("merge hist init");
        for h in sub_handles {
            let sub_hist = h.await.unwrap();
            merged.add(&sub_hist).unwrap();
        }

        // Stop server
        server_handle.abort();
        let _ = server_handle.await;

        // Report percentile summary (microseconds)
        let p50 = merged.value_at_quantile(0.50);
        let p90 = merged.value_at_quantile(0.90);
        let p99 = merged.value_at_quantile(0.99);
        let p999 = merged.value_at_quantile(0.999);
        let max = merged.max();
        let mean = merged.mean(); // f64 microseconds

        // Throughput using wall time is still useful to track
        let total_duration = start.elapsed();
        let total_fanout = NUM_MESSAGES * NUM_SUBSCRIBERS;
        let throughput = total_fanout as f64 / total_duration.as_secs_f64();

        info!(
            "✅ BlipMQ → pub_time={:?}, total_time={:?}, throughput={:.2} msg/s",
            publish_duration, total_duration, throughput
        );
        println!(
            "\n=== Latency Percentiles (pub→sub, µs) ===\n\
             p50:  {}\n\
             p90:  {}\n\
             p99:  {}\n\
             p99.9:{}\n\
             max:  {}\n\
             mean: {:.2}\n",
            p50, p90, p99, p999, max, mean
        );

        (throughput, mean) // return mean latency in µs from histogram
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

    println!("\n=== Latency Summary (mean µs) ===");
    println!("BlipMQ mean latency (hist): {:.2} µs", latency_blip);
    println!("NATS mean latency (wall):  {:.2} µs", latency_nats);
}

criterion_group!(benches, qos0_network_benchmark);
criterion_main!(benches);
