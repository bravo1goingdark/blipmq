use std::error::Error;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use blipmq_auth::StaticApiKeyValidator;
use blipmq_core::{Broker, BrokerConfig, QoSLevel};
use blipmq_net::{
    encode_frame, try_decode_frame, AckPayload, AuthPayload, BrokerHandler, Frame, FrameType,
    HelloPayload, NetworkConfig, PollPayload, PublishPayload, Server, SubscribePayload,
    PROTOCOL_VERSION,
};
use blipmq_wal::{WalConfig, WriteAheadLog};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use clap::Parser;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{watch, Mutex};
use tokio::time;
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Clone, Copy, Debug)]
enum Mode {
    Qos0InMem,
    Qos1InMem,
    Durable,
    Scaled,
}

#[derive(Parser, Debug)]
#[command(
    name = "blipmq_bench",
    about = "BlipMQ in-process performance harness using the binary protocol"
)]
struct Cli {
    /// Benchmark mode: qos0, qos1, durable, scaled.
    #[arg(long, value_parser = ["qos0", "qos1", "durable", "scaled"], default_value = "qos1")]
    mode: String,

    /// Number of publisher clients.
    #[arg(long, default_value_t = 1)]
    publishers: usize,

    /// Number of subscriber clients.
    #[arg(long, default_value_t = 1)]
    subscribers: usize,

    /// Message payload size in bytes (must be >= 8).
    #[arg(long, default_value_t = 256)]
    message_size: usize,

    /// QoS mode: 0 (AtMostOnce) or 1 (AtLeastOnce). Overridden by mode when applicable.
    #[arg(long, default_value_t = 1)]
    qos: u8,

    /// WAL usage: on | off. Overridden by some modes.
    #[arg(long, value_parser = ["on", "off"], default_value = "off")]
    use_wal: String,

    /// Benchmark duration in seconds. If zero, only total-messages is used.
    #[arg(long, default_value_t = 10)]
    duration_secs: u64,

    /// Total messages to publish across all publishers. If zero, only duration is used.
    #[arg(long, default_value_t = 0)]
    total_messages: u64,
}

#[derive(Debug)]
struct BenchConfig {
    mode: Mode,
    publishers: usize,
    subscribers: usize,
    qos: QoSLevel,
    use_wal: bool,
    message_size: usize,
    duration: Option<Duration>,
    total_messages: Option<u64>,
}

#[derive(Default)]
struct Stats {
    latencies_ns: Vec<u64>,
    received: u64,
}

struct BenchClient {
    stream: TcpStream,
    buf: BytesMut,
    next_cid: u64,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();
}

fn parse_mode(s: &str) -> Mode {
    match s {
        "qos0" => Mode::Qos0InMem,
        "qos1" => Mode::Qos1InMem,
        "durable" => Mode::Durable,
        "scaled" => Mode::Scaled,
        _ => Mode::Qos1InMem,
    }
}

fn effective_config(cli: &Cli) -> Result<BenchConfig, Box<dyn Error>> {
    if cli.message_size < 8 {
        return Err("message_size must be at least 8 bytes".into());
    }

    let mode = parse_mode(&cli.mode);

    let mut publishers = cli.publishers.max(1);
    let mut subscribers = cli.subscribers.max(1);

    let mut qos = match cli.qos {
        0 => QoSLevel::AtMostOnce,
        1 => QoSLevel::AtLeastOnce,
        _ => return Err("qos must be 0 or 1".into()),
    };

    let mut use_wal = matches!(cli.use_wal.to_lowercase().as_str(), "on");

    match mode {
        Mode::Qos0InMem => {
            qos = QoSLevel::AtMostOnce;
            use_wal = false;
        }
        Mode::Qos1InMem => {
            qos = QoSLevel::AtLeastOnce;
            use_wal = false;
        }
        Mode::Durable => {
            qos = QoSLevel::AtLeastOnce;
            use_wal = true;
        }
        Mode::Scaled => {
            if publishers == 1 {
                publishers = 4;
            }
            if subscribers == 1 {
                subscribers = 4;
            }
        }
    }

    let duration = if cli.duration_secs > 0 {
        Some(Duration::from_secs(cli.duration_secs))
    } else {
        None
    };

    let total_messages = if cli.total_messages > 0 {
        Some(cli.total_messages)
    } else {
        None
    };

    if duration.is_none() && total_messages.is_none() {
        return Err("either --duration-secs or --total-messages must be > 0".into());
    }

    Ok(BenchConfig {
        mode,
        publishers,
        subscribers,
        qos,
        use_wal,
        message_size: cli.message_size,
        duration,
        total_messages,
    })
}

async fn start_server(broker: Arc<Broker>, api_key: &str) -> (SocketAddr, watch::Sender<bool>) {
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .expect("bind ephemeral failed");
    let addr = listener
        .local_addr()
        .expect("failed to read local addr for listener");
    drop(listener);

    let handler = BrokerHandler::new(broker);
    let auth_validator = Arc::new(StaticApiKeyValidator::from_keys([api_key.to_string()]));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let server = Server::new(
        NetworkConfig { bind_addr: addr },
        handler,
        auth_validator,
        shutdown_rx,
    );

    tokio::spawn(async move {
        let _ = server.start().await;
    });

    // Give the server a moment to start accepting.
    time::sleep(Duration::from_millis(50)).await;

    (addr, shutdown_tx)
}

impl BenchClient {
    async fn connect(addr: SocketAddr, api_key: &str) -> Result<Self, Box<dyn Error>> {
        let stream = TcpStream::connect(addr).await?;
        let mut client = Self {
            stream,
            buf: BytesMut::with_capacity(4096),
            next_cid: 1,
        };
        client.handshake(api_key).await?;
        Ok(client)
    }

    fn next_correlation_id(&mut self) -> u64 {
        let id = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        id
    }

    async fn handshake(&mut self, api_key: &str) -> Result<(), Box<dyn Error>> {
        // HELLO
        let hello_payload = HelloPayload {
            protocol_version: PROTOCOL_VERSION,
        }
        .encode();
        let hello_frame = Frame {
            msg_type: FrameType::Hello,
            correlation_id: self.next_correlation_id(),
            payload: hello_payload,
        };
        self.send_frame(&hello_frame).await?;
        let resp = self.recv_frame().await.ok_or("no HELLO response")?;
        if resp.msg_type != FrameType::Ack {
            return Err("expected ACK to HELLO".into());
        }

        // AUTH
        let auth_payload = AuthPayload {
            api_key: api_key.to_string(),
        }
        .encode()?;
        let auth_frame = Frame {
            msg_type: FrameType::Auth,
            correlation_id: self.next_correlation_id(),
            payload: auth_payload,
        };
        self.send_frame(&auth_frame).await?;
        let resp = self.recv_frame().await.ok_or("no AUTH response")?;
        if resp.msg_type != FrameType::Ack {
            return Err("expected ACK to AUTH".into());
        }

        Ok(())
    }

    async fn send_frame(&mut self, frame: &Frame) -> Result<(), Box<dyn Error>> {
        let mut buf = BytesMut::new();
        encode_frame(frame, &mut buf)?;
        self.stream.write_all(&buf).await?;
        Ok(())
    }

    async fn recv_frame(&mut self) -> Option<Frame> {
        loop {
            if let Some(frame) = try_decode_frame(&mut self.buf).ok().flatten() {
                return Some(frame);
            }

            let n = self.stream.read_buf(&mut self.buf).await.ok()?;
            if n == 0 {
                return None;
            }
        }
    }

    async fn recv_frame_timeout(&mut self, timeout: Duration) -> Option<Frame> {
        (time::timeout(timeout, self.recv_frame()).await).unwrap_or_default()
    }

    async fn subscribe(&mut self, topic: &str, qos: QoSLevel) -> Result<u64, Box<dyn Error>> {
        let qos_byte = match qos {
            QoSLevel::AtMostOnce => 0,
            QoSLevel::AtLeastOnce => 1,
        };
        let payload = SubscribePayload {
            topic: topic.to_string(),
            qos: qos_byte,
        }
        .encode()?;
        let frame = Frame {
            msg_type: FrameType::Subscribe,
            correlation_id: self.next_correlation_id(),
            payload,
        };
        self.send_frame(&frame).await?;
        let resp = self.recv_frame().await.ok_or("no SUBSCRIBE response")?;
        if resp.msg_type != FrameType::Ack {
            return Err("expected ACK to SUBSCRIBE".into());
        }
        let ack = AckPayload::decode(&resp.payload)?;
        if ack.subscription_id == 0 {
            return Err("subscription_id must be non-zero".into());
        }
        Ok(ack.subscription_id)
    }

    async fn publish(
        &mut self,
        topic: &str,
        qos: QoSLevel,
        message: Bytes,
    ) -> Result<(), Box<dyn Error>> {
        let qos_byte = match qos {
            QoSLevel::AtMostOnce => 0,
            QoSLevel::AtLeastOnce => 1,
        };
        let payload = PublishPayload {
            topic: topic.to_string(),
            qos: qos_byte,
            message,
        }
        .encode()?;
        let frame = Frame {
            msg_type: FrameType::Publish,
            correlation_id: self.next_correlation_id(),
            payload,
        };
        self.send_frame(&frame).await
    }

    async fn poll(&mut self, sub_id: u64, timeout: Duration) -> Option<(Frame, PublishPayload)> {
        let payload = PollPayload {
            subscription_id: sub_id,
        }
        .encode()
        .ok()?;
        let frame = Frame {
            msg_type: FrameType::Poll,
            correlation_id: self.next_correlation_id(),
            payload,
        };
        if self.send_frame(&frame).await.is_err() {
            return None;
        }

        let resp = self.recv_frame_timeout(timeout).await?;
        match resp.msg_type {
            FrameType::Publish => {
                let payload = PublishPayload::decode(&resp.payload).ok()?;
                Some((resp, payload))
            }
            _ => None,
        }
    }

    async fn ack(&mut self, sub_id: u64, delivery_cid: u64) -> Result<(), Box<dyn Error>> {
        let payload = AckPayload {
            subscription_id: sub_id,
        }
        .encode()?;
        let frame = Frame {
            msg_type: FrameType::Ack,
            correlation_id: delivery_cid,
            payload,
        };
        self.send_frame(&frame).await
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    init_tracing();
    let cli = Cli::parse();
    let cfg = effective_config(&cli)?;

    println!("=== BlipMQ bench ===");
    println!(
        "mode: {:?}, publishers: {}, subscribers: {}, qos: {:?}, wal: {}",
        cfg.mode, cfg.publishers, cfg.subscribers, cfg.qos, cfg.use_wal
    );

    // WAL & broker setup.
    let wal = if cfg.use_wal {
        let mut path = std::env::temp_dir();
        path.push("blipmq_bench.wal");
        let _ = std::fs::remove_file(&path);
        let wal_cfg = WalConfig::default();
        let wal = WriteAheadLog::open_with_config(&path, wal_cfg).await?;
        Some(Arc::new(wal))
    } else {
        None
    };

    let broker_cfg = BrokerConfig {
        default_qos: cfg.qos,
        message_ttl: Duration::from_secs(60),
        per_subscriber_queue_capacity: 4096,
        max_retries: 3,
        retry_base_delay: Duration::from_millis(50),
    };

    let broker = if let Some(wal) = wal.clone() {
        Arc::new(Broker::new_with_wal(broker_cfg, wal))
    } else {
        Arc::new(Broker::new(broker_cfg))
    };

    let api_key = "bench-key";
    let (addr, shutdown_tx) = start_server(broker.clone(), api_key).await;

    // Shared stats.
    let stats = Arc::new(Mutex::new(Stats::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let sent_counter = Arc::new(AtomicU64::new(0));

    let start = Instant::now();
    let deadline = cfg.duration.map(|d| start + d);

    // Spawn publishers.
    let mut pub_handles = Vec::new();
    for _ in 0..cfg.publishers {
        let addr_copy = addr;
        let api_key = api_key.to_string();
        let message_size = cfg.message_size;
        let qos = cfg.qos;
        let sent_counter = sent_counter.clone();
        let total_messages = cfg.total_messages;
        let deadline_opt = deadline;
        let start_instant = start;

        let handle = tokio::spawn(async move {
            let mut client = BenchClient::connect(addr_copy, &api_key)
                .await
                .expect("connect failed");

            loop {
                if let Some(deadline) = deadline_opt {
                    if Instant::now() >= deadline {
                        break;
                    }
                }

                if let Some(total) = total_messages {
                    let current = sent_counter.fetch_add(1, Ordering::Relaxed);
                    if current >= total {
                        break;
                    }
                }

                // Build payload: [u64 send_time_ns][padding...].
                let mut buf = BytesMut::with_capacity(message_size);
                let ts_ns = start_instant.elapsed().as_nanos() as u64;
                buf.put_u64_le(ts_ns);
                if message_size > 8 {
                    buf.resize(message_size, 0);
                }
                let payload = buf.freeze();

                client
                    .publish("bench", qos, payload)
                    .await
                    .expect("publish failed");
            }
        });

        pub_handles.push(handle);
    }

    // Spawn subscribers.
    let mut sub_handles = Vec::new();
    for _ in 0..cfg.subscribers {
        let addr_copy = addr;
        let api_key = api_key.to_string();
        let qos = cfg.qos;
        let stats = stats.clone();
        let stop = stop.clone();
        let start_instant = start;

        let handle = tokio::spawn(async move {
            let mut client = BenchClient::connect(addr_copy, &api_key)
                .await
                .expect("connect failed");
            let sub_id = client
                .subscribe("bench", qos)
                .await
                .expect("subscribe failed");

            let mut idle_loops = 0u32;
            loop {
                if stop.load(Ordering::Relaxed) {
                    idle_loops += 1;
                    if idle_loops > 20 {
                        break;
                    }
                }

                if let Some((frame, payload)) = client.poll(sub_id, Duration::from_millis(50)).await
                {
                    idle_loops = 0;

                    if payload.message.len() >= 8 {
                        let mut slice = &payload.message[..8];
                        let mut ts_bytes = [0u8; 8];
                        slice.copy_to_slice(&mut ts_bytes);
                        let ts_ns = u64::from_le_bytes(ts_bytes);
                        let now_ns = start_instant.elapsed().as_nanos() as u64;
                        let latency = now_ns.saturating_sub(ts_ns);

                        let mut s = stats.lock().await;
                        s.latencies_ns.push(latency);
                        s.received = s.received.saturating_add(1);
                    }

                    if qos == QoSLevel::AtLeastOnce {
                        let _ = client.ack(sub_id, frame.correlation_id).await;
                    }
                } else {
                    idle_loops = idle_loops.saturating_add(1);
                    time::sleep(Duration::from_millis(5)).await;
                }
            }
        });

        sub_handles.push(handle);
    }

    // Wait for publishers, then signal stop.
    for h in pub_handles {
        let _ = h.await;
    }
    stop.store(true, Ordering::SeqCst);

    // Wait for subscribers to drain.
    for h in sub_handles {
        let _ = h.await;
    }

    let _ = shutdown_tx.send(true);

    // Summarize results.
    let elapsed = start.elapsed();
    let stats = stats.lock().await;
    let total_msgs = stats.received;
    let secs = elapsed.as_secs_f64();
    let msgs_per_sec = if secs > 0.0 {
        total_msgs as f64 / secs
    } else {
        0.0
    };

    let mut latencies = stats.latencies_ns.clone();
    latencies.sort_unstable();

    let percentile = |p: f64| -> Option<u64> {
        if latencies.is_empty() {
            return None;
        }
        let idx = ((p / 100.0) * (latencies.len() - 1) as f64).round() as usize;
        latencies.get(idx).copied()
    };

    println!("=== BlipMQ bench results ===");
    println!("publishers: {}", cfg.publishers);
    println!("subscribers: {}", cfg.subscribers);
    println!("qos: {:?}", cfg.qos);
    println!("use_wal: {}", cfg.use_wal);
    println!("message_size: {} bytes", cfg.message_size);
    println!("messages received: {total_msgs}");
    println!("elapsed: {secs:.3}s");
    println!("throughput: {msgs_per_sec:.0} msgs/sec");

    if let Some(p50) = percentile(50.0) {
        println!("p50 latency: {:.2} µs", p50 as f64 / 1_000.0);
    }
    if let Some(p95) = percentile(95.0) {
        println!("p95 latency: {:.2} µs", p95 as f64 / 1_000.0);
    }
    if let Some(p99) = percentile(99.0) {
        println!("p99 latency: {:.2} µs", p99 as f64 / 1_000.0);
    }

    Ok(())
}
