use blipmq::config::CONFIG;
use blipmq::core::command::{encode_command, new_pub, new_sub};
use blipmq::core::message::{decode_frame, ServerFrame};
use blipmq::start_broker;

use anyhow::Context;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use hdrhistogram::Histogram;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    runtime::Runtime,
    sync::Barrier,
};
use tracing::{error, info, warn};

fn bench_params() -> (usize, usize, bool) {
    let quick = std::env::var("BLIPMQ_QUICK_BENCH").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false);
    if quick {
        // lighter params for fast validation
        (8, 2_000, true)
    } else {
        (48, 48_000, false)
    }
}

// helper fn to get a timestamp (µs since UNIX epoch)
fn now_micros() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros()
}

async fn run_network_qos0_benchmark_inner() -> anyhow::Result<(f64, f64, Histogram<u64>)> {
    let (num_subs, num_msgs, _) = bench_params();
    let addr = CONFIG.server.bind_addr.as_str();
    let topic = "bench_topic".to_string();
    let barrier = Arc::new(Barrier::new(num_subs + 1));

    let server_handle = tokio::spawn(async move {
        if let Err(e) = start_broker().await {
            error!("Broker failed to start or encountered an error: {:?}", e);
        }
    });

    // brief warmup to let the broker bind
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut sub_handles = Vec::with_capacity(num_subs);
    for i in 0..num_subs {
        let addr = addr.to_string();
        let topic = topic.clone();
        let barrier = barrier.clone();

        sub_handles.push(tokio::spawn(async move {
            let stream = TcpStream::connect(&addr)
                .await
                .with_context(|| format!("Subscriber {} failed to connect to {}", i, addr))?;
            stream
                .set_nodelay(true)
                .context("Failed to set no_delay for subscriber stream")?;
            let (read_half, mut write_half) = stream.into_split();

            let cmd = new_sub(topic.clone());
            let enc = encode_command(&cmd);
            write_half
                .write_all(&(enc.len() as u32).to_be_bytes())
                .await
                .context("Subscriber failed to write length prefix")?;
            write_half
                .write_all(&enc)
                .await
                .context("Subscriber failed to write command")?;
            write_half
                .flush()
                .await
                .context("Subscriber failed to flush command")?;

            let mut reader = BufReader::new(read_half);

            // Read SubAck
            let mut len_buf = [0u8; 4];
            reader
                .read_exact(&mut len_buf)
                .await
                .context("Subscriber failed to read SubAck length")?;
            let frame_len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; frame_len];
            reader
                .read_exact(&mut buf)
                .await
                .context("Subscriber failed to read SubAck payload")?;

            match decode_frame(&buf) {
                Ok(ServerFrame::SubAck(_)) => {}
                Ok(_) => anyhow::bail!("Subscriber {} received invalid SubAck frame", i),
                Err(e) => anyhow::bail!("Subscriber {} failed to decode SubAck: {}", i, e),
            }

            barrier.wait().await;

            let mut received = 0;
            let mut hist = Histogram::<u64>::new(3).unwrap(); // per-subscriber histogram (µs)
            while received < num_msgs {
                let mut len_buf = [0u8; 4];
                if reader.read_exact(&mut len_buf).await.is_err() {
                    warn!("Subscriber {} disconnected or failed to read length.", i);
                    break;
                }
                let msg_len = u32::from_be_bytes(len_buf) as usize;
                let mut msg_buf = vec![0u8; msg_len];
                if reader.read_exact(&mut msg_buf).await.is_err() {
                    warn!("Subscriber {} failed to read payload.", i);
                    break;
                }

                // parse timestamp from payload: "<seq>|<micros_since_epoch>"
                let payload = String::from_utf8_lossy(&msg_buf);
                if let Some(ts_str) = payload.split('|').nth(1) {
                    if let Ok(sent_micros) = ts_str.parse::<u128>() {
                        let now_us = now_micros(); // avoid shadowing the fn name
                        if now_us > sent_micros {
                            let latency_us = (now_us - sent_micros) as u64;
                            let _ = hist.record(latency_us);
                        }
                    }
                }

                received += 1;
            }

            anyhow::ensure!(
                received == num_msgs,
                "Subscriber {} message loss: expected {}, got {}",
                i,
                num_msgs,
                received
            );
            Ok(hist)
        }));
    }

    barrier.wait().await;

    let mut pub_stream = TcpStream::connect(addr)
        .await
        .context("Publisher failed to connect to broker")?;
    pub_stream
        .set_nodelay(true)
        .context("Failed to set no_delay for publisher stream")?;

    let start = Instant::now();
    for i in 0..num_msgs {
        let payload = format!("{}|{}", i, now_micros()); // include timestamp
        let cmd = new_pub(topic.clone(), payload);
        let enc = encode_command(&cmd);
        pub_stream
            .write_all(&(enc.len() as u32).to_be_bytes())
            .await
            .context("Publisher failed to write length prefix")?;
        pub_stream
            .write_all(&enc)
            .await
            .context("Publisher failed to write command")?;
    }
    pub_stream
        .flush()
        .await
        .context("Publisher failed to flush commands")?;
    let publish_duration = start.elapsed();

    let mut merged_hist = Histogram::<u64>::new(3)?; // µs histogram
    for h in sub_handles {
        let sub_hist = h.await.context("Subscriber task failed")??;
        merged_hist.add(&sub_hist)?; // merge histograms
    }

    server_handle.abort();
    let _ = server_handle.await;

    let total_duration = start.elapsed();
    let total_fanout = num_msgs * num_subs;
    let throughput = total_fanout as f64 / total_duration.as_secs_f64();
    let mean_latency_us = total_duration.as_secs_f64() / total_fanout as f64 * 1e6;

    info!(
        "✅ BlipMQ → pub_time={:?}, total_time={:?}, throughput={:.2} msg/s, mean_latency={:.2}µs, p50={}µs, p95={}µs, p99={}µs",
        publish_duration,
        total_duration,
        throughput,
        mean_latency_us,
        merged_hist.value_at_quantile(0.5),
        merged_hist.value_at_quantile(0.95),
        merged_hist.value_at_quantile(0.99)
    );

    Ok((throughput, mean_latency_us, merged_hist))
}

fn run_network_qos0_benchmark() -> (f64, f64, Histogram<u64>) {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        run_network_qos0_benchmark_inner()
            .await
            .unwrap_or_else(|e| {
                error!("BlipMQ benchmark failed: {:?}", e);
                (0.0, 0.0, Histogram::<u64>::new(3).unwrap())
            })
    })
}

// NOTE: Apply the same timestamp+hist approach to NATS when you wire that in.

fn qos0_network_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("qos0_network_tcp");
    let (num_subs, num_msgs, quick) = bench_params();
    group.throughput(Throughput::Elements((num_msgs * num_subs) as u64));
    if quick {
        group.sample_size(10);
        group.measurement_time(Duration::from_secs(5));
    } else {
        group.sample_size(10);
        group.measurement_time(Duration::from_secs(60));
    }

    let mut last_hist: Option<Histogram<u64>> = None;
    let mut latency_blip_mean = 0.0;

    group.bench_function("blipmq_qos0_tcp", |b| {
        b.iter_custom(|iters| {
            // iter_custom MUST return a Duration (total time for `iters` runs)
            let mut total_elapsed = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let (tp, mean_lat, hist) = run_network_qos0_benchmark();
                let elapsed = start.elapsed();
                total_elapsed += elapsed;

                latency_blip_mean = mean_lat;
                last_hist = Some(hist);

                // Optional: print per-iteration summary
                println!(
                    "BlipMQ iter: elapsed={:?}, throughput≈{:.2} msg/s",
                    elapsed, tp
                );
                if quick { break; }
            }
            total_elapsed
        })
    });

    group.finish();

    println!("\n=== Latency Summary (BlipMQ) ===");
    println!(
        "Mean latency (throughput-derived): {:.2} µs",
        latency_blip_mean
    );
    if let Some(h) = last_hist {
        println!(
            "p50={}µs, p95={}µs, p99={}µs",
            h.value_at_quantile(0.50),
            h.value_at_quantile(0.95),
            h.value_at_quantile(0.99)
        );
    }
}

criterion_group!(benches, qos0_network_benchmark);
criterion_main!(benches);
