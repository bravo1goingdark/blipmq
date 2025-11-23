use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use bench_compare::{extract_timestamp_ns, BenchError, LatencyStats};
use clap::Parser;
#[derive(Parser, Debug)]
#[command(
    name = "sub_nats",
    about = "Subscribe to NATS and measure receive throughput/latency"
)]
struct Args {
    /// NATS server URL (e.g., nats://127.0.0.1:4222).
    #[arg(long, default_value = "nats://127.0.0.1:4222")]
    server: String,

    /// Subject to subscribe to.
    #[arg(long, default_value = "bench")]
    subject: String,

    /// Total messages expected across all subscribers.
    #[arg(long, default_value_t = 10000)]
    count: u64,

    /// Parallel subscriber tasks.
    #[arg(long, default_value_t = 1)]
    parallelism: usize,

    /// Use JetStream subscription with ACKs.
    #[arg(long, default_value_t = false)]
    jetstream: bool,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), BenchError> {
    let args = Args::parse();
    let start = Instant::now();
    let total_target = args.count * args.parallelism as u64;
    let received = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();

    for _ in 0..args.parallelism {
        let server = args.server.clone();
        let subject = args.subject.clone();
        let received = received.clone();
        let jetstream = args.jetstream;

        let handle = if jetstream {
            tokio::task::spawn_blocking(move || {
                let nc = nats::connect(&server)?;
                let js = nats::jetstream::new(nc);
                let sub = js.subscribe(&subject)?;
                let mut stats = LatencyStats::default();
                while received.load(Ordering::Relaxed) < total_target {
                    match sub.next_timeout(Duration::from_millis(200)) {
                        Ok(msg) => {
                            if let Some(sent_ns) = extract_timestamp_ns(&msg.data) {
                                let latency = bench_compare::now_ns().saturating_sub(sent_ns);
                                stats.record_ns(latency);
                            }
                            received.fetch_add(1, Ordering::Relaxed);
                            let _ = msg.ack();
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::TimedOut => continue,
                        Err(_) => break,
                    }
                }
                Ok::<LatencyStats, BenchError>(stats)
            })
        } else {
            tokio::spawn(async move {
                let nc = nats::asynk::connect(&server).await?;
                let sub = nc.subscribe(&subject).await?;
                let mut stats = LatencyStats::default();
                while received.load(Ordering::Relaxed) < total_target {
                    match tokio::time::timeout(Duration::from_millis(200), sub.next()).await {
                        Ok(Some(msg)) => {
                            if let Some(sent_ns) = extract_timestamp_ns(&msg.data) {
                                let latency = bench_compare::now_ns().saturating_sub(sent_ns);
                                stats.record_ns(latency);
                            }
                            received.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(None) => break,
                        Err(_) => continue,
                    }
                }
                Ok::<LatencyStats, BenchError>(stats)
            })
        };

        handles.push(handle);
    }

    let mut combined = LatencyStats::default();
    for handle in handles {
        let stats = handle
            .await
            .map_err(|e| format!("subscriber join error: {e}"))??;
        combined.merge(stats);
    }

    let elapsed = start.elapsed();
    let recvd = received.load(Ordering::SeqCst);
    let throughput = if elapsed.as_secs_f64() > 0.0 {
        recvd as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };
    let lat = combined.summarize();

    println!("Received {recvd} messages in {:.3}s", elapsed.as_secs_f64());
    println!("Throughput: {throughput:.0} msgs/sec");
    println!("p50 latency: {:.2}us", lat.p50_us);
    println!("p95 latency: {:.2}us", lat.p95_us);
    println!("p99 latency: {:.2}us", lat.p99_us);

    Ok(())
}
