use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Instant;

use bench_compare::{build_payload, BenchError, LatencyStats};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "pub_nats",
    about = "Publish messages to NATS and measure throughput/latency"
)]
struct Args {
    /// NATS server URL (e.g., nats://127.0.0.1:4222).
    #[arg(long, default_value = "nats://127.0.0.1:4222")]
    server: String,

    /// Subject to publish to.
    #[arg(long, default_value = "bench")]
    subject: String,

    /// Total messages to publish across all publishers.
    #[arg(long, default_value_t = 10000)]
    count: u64,

    /// Message payload size in bytes.
    #[arg(long, default_value_t = 1024)]
    message_size: usize,

    /// Parallel publisher tasks.
    #[arg(long, default_value_t = 1)]
    parallelism: usize,

    /// Use JetStream publish/ack path.
    #[arg(long, default_value_t = false)]
    jetstream: bool,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), BenchError> {
    let args = Args::parse();
    let counter = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    let start = Instant::now();

    for _ in 0..args.parallelism {
        let server = args.server.clone();
        let subject = args.subject.clone();
        let total = args.count;
        let counter = counter.clone();
        let message_size = args.message_size;
        let jetstream = args.jetstream;

        let handle = if jetstream {
            tokio::task::spawn_blocking(move || {
                let nc = nats::connect(&server)?;
                let js = nats::jetstream::new(nc);
                let mut stats = LatencyStats::default();
                loop {
                    let idx = counter.fetch_add(1, Ordering::Relaxed);
                    if idx >= total {
                        break;
                    }
                    let payload = build_payload(message_size);
                    let t0 = Instant::now();
                    js.publish(&subject, payload)?;
                    stats.record_ns(t0.elapsed().as_nanos() as u64);
                }
                Ok::<LatencyStats, BenchError>(stats)
            })
        } else {
            tokio::spawn(async move {
                let nc = nats::asynk::connect(&server).await?;
                let mut stats = LatencyStats::default();
                loop {
                    let idx = counter.fetch_add(1, Ordering::Relaxed);
                    if idx >= total {
                        break;
                    }
                    let payload = build_payload(message_size);
                    let t0 = Instant::now();
                    nc.publish(&subject, payload).await?;
                    stats.record_ns(t0.elapsed().as_nanos() as u64);
                }
                nc.flush().await?;
                Ok::<LatencyStats, BenchError>(stats)
            })
        };

        handles.push(handle);
    }

    let mut combined = LatencyStats::default();
    for handle in handles {
        let stats = handle
            .await
            .map_err(|e| format!("publisher join error: {e}"))??;
        combined.merge(stats);
    }

    let elapsed = start.elapsed();
    let sent = counter.load(Ordering::SeqCst);
    let throughput = if elapsed.as_secs_f64() > 0.0 {
        sent as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };
    let lat = combined.summarize();

    println!("Published {sent} messages in {:.3}s", elapsed.as_secs_f64());
    println!("Throughput: {throughput:.0} msgs/sec");
    println!("p50 publish latency: {:.2}us", lat.p50_us);
    println!("p95 publish latency: {:.2}us", lat.p95_us);
    println!("p99 publish latency: {:.2}us", lat.p99_us);

    Ok(())
}
