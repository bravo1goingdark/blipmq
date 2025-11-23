use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Instant;

use bench_compare::{build_payload, BenchError, BmqClient, LatencyStats};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "pub_bmq",
    about = "Publish messages to BlipMQ and measure throughput/latency"
)]
struct Args {
    /// BlipMQ address (ip:port).
    #[arg(long, default_value = "127.0.0.1:5555")]
    addr: String,

    /// API key to use during AUTH.
    #[arg(long, default_value = "bench-key")]
    api_key: String,

    /// Topic/subject to publish to.
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

    /// QoS level (0 or 1).
    #[arg(long, default_value_t = 0)]
    qos: u8,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), BenchError> {
    let args = Args::parse();
    let addr: SocketAddr = args
        .addr
        .parse()
        .map_err(|e| format!("invalid addr: {e}"))?;

    let counter = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    let start = Instant::now();

    for _ in 0..args.parallelism {
        let api_key = args.api_key.clone();
        let subject = args.subject.clone();
        let counter = counter.clone();
        let total = args.count;
        let message_size = args.message_size;
        let qos = args.qos;

        let handle = tokio::spawn(async move {
            let mut client = BmqClient::connect(addr, &api_key).await?;
            let mut stats = LatencyStats::default();
            loop {
                let idx = counter.fetch_add(1, Ordering::Relaxed);
                if idx >= total {
                    break;
                }
                let payload = build_payload(message_size);
                let t0 = Instant::now();
                client.publish(&subject, qos, payload).await?;
                stats.record_ns(t0.elapsed().as_nanos() as u64);
            }
            Ok::<LatencyStats, BenchError>(stats)
        });

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
