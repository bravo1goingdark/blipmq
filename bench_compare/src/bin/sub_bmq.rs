use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use bench_compare::{extract_timestamp_ns, BenchError, BmqClient, LatencyStats};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "sub_bmq",
    about = "Subscribe to BlipMQ and measure receive throughput/latency"
)]
struct Args {
    /// BlipMQ address (ip:port).
    #[arg(long, default_value = "127.0.0.1:5555")]
    addr: String,

    /// API key to use during AUTH.
    #[arg(long, default_value = "bench-key")]
    api_key: String,

    /// Topic/subject to subscribe to.
    #[arg(long, default_value = "bench")]
    subject: String,

    /// Total messages expected across all subscribers.
    #[arg(long, default_value_t = 10000)]
    count: u64,

    /// Parallel subscriber tasks.
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

    let total_target = args.count * args.parallelism as u64;
    let received = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();

    for _ in 0..args.parallelism {
        let api_key = args.api_key.clone();
        let subject = args.subject.clone();
        let qos = args.qos;
        let received = received.clone();

        let handle = tokio::spawn(async move {
            let mut client = BmqClient::connect(addr, &api_key).await?;
            let sub_id = client.subscribe(&subject, qos).await?;
            let mut stats = LatencyStats::default();
            // Start the receive timer at the first message, not at
            // task start. This excludes connection setup, subscribe
            // handshake, and any wait for the publisher to begin —
            // matching `nats bench`'s methodology and giving an
            // apples-to-apples receive-throughput number.
            let mut first_msg_at: Option<Instant> = None;

            while received.load(Ordering::Relaxed) < total_target {
                // v2 push: read DELIVER frames as they arrive. No
                // per-poll round-trip; the broker fans messages
                // straight into the connection's writer task buffer
                // and the kernel buffers them on the socket.
                match client.next_delivery(Duration::from_secs(5)).await {
                    Some((frame, payload)) => {
                        if first_msg_at.is_none() {
                            first_msg_at = Some(Instant::now());
                        }
                        if let Some(sent_ns) = extract_timestamp_ns(&payload.message) {
                            let latency = bench_compare::now_ns().saturating_sub(sent_ns);
                            stats.record_ns(latency);
                        }
                        received.fetch_add(1, Ordering::Relaxed);
                        if qos == 1 {
                            // ACK the delivery_tag (carried in
                            // frame.correlation_id by the wire format).
                            let _ = client.ack(sub_id, frame.correlation_id).await;
                        }
                    }
                    None => {
                        // Timeout / EOF — bail out so we don't loop
                        // forever on a stalled stream.
                        break;
                    }
                }
            }

            let elapsed = first_msg_at.map(|t| t.elapsed()).unwrap_or_default();
            Ok::<(LatencyStats, Duration), BenchError>((stats, elapsed))
        });

        handles.push(handle);
    }

    let mut combined = LatencyStats::default();
    let mut max_elapsed = Duration::default();
    for handle in handles {
        let (stats, elapsed) = handle
            .await
            .map_err(|e| format!("subscriber join error: {e}"))??;
        combined.merge(stats);
        if elapsed > max_elapsed {
            max_elapsed = elapsed;
        }
    }

    let elapsed = max_elapsed;
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
