use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};

use bench_compare::{
    run_case, BenchError, BenchResult, BenchmarkCase, BrokerKind, ResourceMonitor,
};

#[derive(Debug, Copy, Clone, ValueEnum, PartialEq, Eq)]
enum BrokerChoice {
    Nats,
    Blipmq,
    All,
}

#[derive(Parser, Debug)]
#[command(name = "bench_compare", about = "Benchmark BlipMQ vs NATS")]
struct Args {
    /// Broker to run benchmarks against.
    #[arg(long, default_value = "all", value_enum)]
    broker: BrokerChoice,

    /// NATS connection URL (e.g., nats://127.0.0.1:4222).
    #[arg(long, default_value = "nats://127.0.0.1:4222")]
    nats_url: String,

    /// BlipMQ address (ip:port).
    #[arg(long, default_value = "127.0.0.1:5555")]
    addr: String,

    /// BlipMQ API key for AUTH.
    #[arg(long, default_value = "bench-key")]
    api_key: String,

    /// Subject/topic to use for all scenarios.
    #[arg(long, default_value = "bench")]
    subject: String,

    /// Total messages to publish per scenario (shared across publishers).
    #[arg(long, default_value_t = 10000)]
    messages: u64,

    /// Enable JetStream scenarios for NATS as QoS1.
    #[arg(long, default_value_t = false)]
    jetstream: bool,

    /// Sampling interval in milliseconds for CPU/memory monitor.
    #[arg(long, default_value_t = 500)]
    monitor_interval_ms: u64,

    /// Optional NATS broker PID to monitor.
    #[arg(long)]
    nats_pid: Option<u32>,

    /// Optional BlipMQ broker PID to monitor.
    #[arg(long)]
    pid: Option<u32>,

    /// Process name hint for NATS (used if PID is not provided).
    #[arg(long, default_value = "nats-server")]
    nats_process: String,

    /// Process name hint for BlipMQ (used if PID is not provided).
    #[arg(long, default_value = "blipmqd")]
    process: String,

    /// Output directory for results.csv and results.json.
    #[arg(long, default_value = "bench_compare/results")]
    results_dir: String,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), BenchError> {
    let args = Args::parse();
    let addr = args
        .addr
        .parse()
        .map_err(|e| format!("invalid blipmq addr: {e}"))?;
    let interval = Duration::from_millis(args.monitor_interval_ms.max(50));

    let mut results = Vec::<BenchResult>::new();

    let message_sizes = [100usize, 1024, 4096];
    let publisher_counts = [1usize, 4, 16];
    let subscriber_counts = [1usize, 4, 16];

    for &message_size in &message_sizes {
        for &publishers in &publisher_counts {
            for &subscribers in &subscriber_counts {
                if args.broker == BrokerChoice::All || args.broker == BrokerChoice::Nats {
                    let monitor = ResourceMonitor::new(
                        Some(args.nats_process.clone()),
                        args.nats_pid,
                        interval,
                    );

                    let case = BenchmarkCase {
                        broker: BrokerKind::NatsCore,
                        subject: args.subject.clone(),
                        message_size,
                        publishers,
                        subscribers,
                        total_messages: args.messages,
                        nats_url: args.nats_url.clone(),
                        addr,
                        api_key: args.api_key.clone(),
                        monitor: Some(monitor),
                    };
                    println!(
                        "Running NATS core: size={} pubs={} subs={} msgs={}",
                        message_size, publishers, subscribers, args.messages
                    );
                    let res = run_case(case).await?;
                    report(&res);
                    results.push(res);

                    if args.jetstream {
                        let monitor = ResourceMonitor::new(
                            Some(args.nats_process.clone()),
                            args.nats_pid,
                            interval,
                        );
                        let case = BenchmarkCase {
                            broker: BrokerKind::NatsJetStream,
                            subject: args.subject.clone(),
                            message_size,
                            publishers,
                            subscribers,
                            total_messages: args.messages,
                            nats_url: args.nats_url.clone(),
                            addr,
                            api_key: args.api_key.clone(),
                            monitor: Some(monitor),
                        };
                        println!(
                            "Running NATS JetStream: size={} pubs={} subs={} msgs={}",
                            message_size, publishers, subscribers, args.messages
                        );
                        let res = run_case(case).await?;
                        report(&res);
                        results.push(res);
                    }
                }

                if args.broker == BrokerChoice::All || args.broker == BrokerChoice::Blipmq {
                    let monitor0 =
                        ResourceMonitor::new(Some(args.process.clone()), args.pid, interval);
                    let case0 = BenchmarkCase {
                        broker: BrokerKind::BlipMqQos0,
                        subject: args.subject.clone(),
                        message_size,
                        publishers,
                        subscribers,
                        total_messages: args.messages,
                        nats_url: args.nats_url.clone(),
                        addr,
                        api_key: args.api_key.clone(),
                        monitor: Some(monitor0),
                    };
                    println!(
                        "Running BlipMQ QoS0: size={} pubs={} subs={} msgs={}",
                        message_size, publishers, subscribers, args.messages
                    );
                    let res0 = run_case(case0).await?;
                    report(&res0);
                    results.push(res0);

                    let monitor1 =
                        ResourceMonitor::new(Some(args.process.clone()), args.pid, interval);
                    let case1 = BenchmarkCase {
                        broker: BrokerKind::BlipMqQos1,
                        subject: args.subject.clone(),
                        message_size,
                        publishers,
                        subscribers,
                        total_messages: args.messages,
                        nats_url: args.nats_url.clone(),
                        addr,
                        api_key: args.api_key.clone(),
                        monitor: Some(monitor1),
                    };
                    println!(
                        "Running BlipMQ QoS1: size={} pubs={} subs={} msgs={}",
                        message_size, publishers, subscribers, args.messages
                    );
                    let res1 = run_case(case1).await?;
                    report(&res1);
                    results.push(res1);
                }
            }
        }
    }

    let results_dir = PathBuf::from(args.results_dir);
    if !results_dir.exists() {
        std::fs::create_dir_all(&results_dir)?;
    }
    let csv_path = results_dir.join("results.csv");
    let json_path = results_dir.join("results.json");
    bench_compare::write_results_csv(&csv_path, &results)?;
    bench_compare::write_results_json(&json_path, &results)?;

    println!(
        "Results saved to {} and {}",
        csv_path.display(),
        json_path.display()
    );
    Ok(())
}

fn report(res: &BenchResult) {
    println!(
        "{} {:<8} size={} pubs={} subs={} msgs={} thr={:.0} msg/s p50={:.2}us p95={:.2}us p99={:.2}us cpu={:.1}% mem={} bytes",
        res.broker,
        res.qos,
        res.message_size,
        res.publishers,
        res.subscribers,
        res.total_messages,
        res.throughput_mps,
        res.latency_p50_us,
        res.latency_p95_us,
        res.latency_p99_us,
        res.cpu_percent,
        res.memory_bytes,
    );
}
