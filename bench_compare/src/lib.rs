use std::error::Error;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::{BufMut, Bytes, BytesMut};
use net::{
    encode_frame, try_decode_frame, AckPayload, AuthPayload, DeliverPayload, Frame, FrameType,
    HelloPayload, PublishPayload, SubscribePayload, PROTOCOL_VERSION,
};
use serde::Serialize;
use sysinfo::Pid;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

pub type BenchError = Box<dyn Error + Send + Sync>;

#[derive(Clone, Debug, Serialize)]
pub struct BenchResult {
    pub broker: String,
    pub qos: String,
    pub message_size: usize,
    pub publishers: usize,
    pub subscribers: usize,
    pub total_messages: u64,
    pub throughput_mps: f64,
    pub latency_p50_us: f64,
    pub latency_p95_us: f64,
    pub latency_p99_us: f64,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
}

#[derive(Default, Clone, Debug)]
pub struct LatencyStats {
    pub samples_ns: Vec<u64>,
}

impl LatencyStats {
    pub fn record_ns(&mut self, value: u64) {
        self.samples_ns.push(value);
    }

    pub fn merge(&mut self, other: LatencyStats) {
        self.samples_ns.extend(other.samples_ns);
    }

    pub fn summarize(&self) -> LatencySummary {
        if self.samples_ns.is_empty() {
            return LatencySummary::default();
        }

        let mut sorted = self.samples_ns.clone();
        sorted.sort_unstable();

        LatencySummary {
            p50_us: percentile(&sorted, 50.0),
            p95_us: percentile(&sorted, 95.0),
            p99_us: percentile(&sorted, 99.0),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct LatencySummary {
    pub p50_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
}

fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted.len().saturating_sub(1) as f64)).round() as usize;
    let ns = sorted.get(idx).copied().unwrap_or(0);
    ns as f64 / 1_000.0
}

pub fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

pub fn build_payload(message_size: usize) -> Bytes {
    let ts = now_ns();
    let mut buf = BytesMut::with_capacity(message_size.max(8));
    buf.put_u64_le(ts);
    if message_size > 8 {
        buf.resize(message_size, 0);
    }
    buf.freeze()
}

pub fn extract_timestamp_ns(data: &[u8]) -> Option<u64> {
    if data.len() < 8 {
        return None;
    }
    let mut ts_bytes = [0u8; 8];
    ts_bytes.copy_from_slice(&data[..8]);
    Some(u64::from_le_bytes(ts_bytes))
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ResourceUsage {
    pub avg_cpu_percent: f32,
    pub max_memory_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ResourceMonitor {
    process_name: Option<String>,
    pid: Option<u32>,
    interval: Duration,
}

impl ResourceMonitor {
    pub fn new(process_name: Option<String>, pid: Option<u32>, interval: Duration) -> Self {
        Self {
            process_name,
            pid,
            interval,
        }
    }

    pub async fn measure<Fut, T>(&self, fut: Fut) -> Result<(T, ResourceUsage), BenchError>
    where
        Fut: std::future::Future<Output = Result<T, BenchError>> + Send,
        T: Send + 'static,
    {
        let mut system = sysinfo::System::new_all();
        let mut cpu_samples = Vec::new();
        let mut mem_samples = Vec::new();
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let name = self.process_name.clone();
        let pid = self.pid.map(Pid::from_u32);
        let interval = self.interval;

        let handle = tokio::spawn(async move {
            loop {
                if *stop_rx.borrow() {
                    break;
                }

                system.refresh_processes();
                system.refresh_cpu();

                let mut cpu_total = 0.0f32;
                let mut mem_total = 0u64;

                if let Some(pid) = pid {
                    if let Some(proc) = system.process(pid) {
                        cpu_total += proc.cpu_usage();
                        mem_total += proc.memory();
                    }
                }

                if cpu_total == 0.0 && mem_total == 0 {
                    if let Some(ref pname) = name {
                        for proc in system.processes_by_name(pname) {
                            cpu_total += proc.cpu_usage();
                            mem_total += proc.memory();
                        }
                    }
                }

                if cpu_total > 0.0 || mem_total > 0 {
                    cpu_samples.push(cpu_total);
                    mem_samples.push(mem_total);
                }

                tokio::time::sleep(interval).await;
            }

            let avg_cpu = if cpu_samples.is_empty() {
                0.0
            } else {
                cpu_samples.iter().sum::<f32>() / cpu_samples.len() as f32
            };
            let max_mem = mem_samples.into_iter().max().unwrap_or(0);

            Ok::<ResourceUsage, BenchError>(ResourceUsage {
                avg_cpu_percent: avg_cpu,
                max_memory_bytes: max_mem,
            })
        });

        let output = fut.await;
        let _ = stop_tx.send(true);
        let usage = handle
            .await
            .map_err(|e| format!("resource monitor join error: {e}"))??;

        output.map(|res| (res, usage))
    }
}

pub struct BmqClient {
    stream: TcpStream,
    buf: BytesMut,
    next_cid: u64,
    /// Outbound write buffer for batched `publish_batched` calls.
    /// Frames accumulate here until `flush_publishes` is called or
    /// the buffer exceeds `BATCH_FLUSH_BYTES`. NATS-style pipelining:
    /// many PUBLISH frames per TCP `write`, amortizing the syscall.
    out: BytesMut,
}

/// Threshold at which `publish_batched` triggers an automatic flush.
/// 64 KiB is a common TCP write granule on Linux loopback (one
/// `tcp_sendmsg` page) and roughly tracks the kernel's default
/// `tcp_wmem` low-water mark, so we keep latency bounded while still
/// amortizing the syscall over many small frames.
pub const BATCH_FLUSH_BYTES: usize = 64 * 1024;

impl BmqClient {
    pub async fn connect(addr: SocketAddr, api_key: &str) -> Result<Self, BenchError> {
        let stream = TcpStream::connect(addr).await?;
        // Disable Nagle so the writer-side batching is what controls
        // write granularity; otherwise we'd pay both Nagle's delay
        // AND batch latency.
        let _ = stream.set_nodelay(true);
        let mut client = Self {
            stream,
            buf: BytesMut::with_capacity(4096),
            next_cid: 1,
            out: BytesMut::with_capacity(BATCH_FLUSH_BYTES * 2),
        };
        client.handshake(api_key).await?;
        Ok(client)
    }

    fn next_correlation_id(&mut self) -> u64 {
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        cid
    }

    async fn handshake(&mut self, api_key: &str) -> Result<(), BenchError> {
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

    async fn send_frame(&mut self, frame: &Frame) -> Result<(), BenchError> {
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

    pub async fn subscribe(&mut self, topic: &str, qos: u8) -> Result<u64, BenchError> {
        let payload = SubscribePayload {
            topic: topic.to_string(),
            qos,
            group: None,
            from_offset: None,
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

    pub async fn publish(
        &mut self,
        topic: &str,
        qos: u8,
        message: Bytes,
    ) -> Result<(), BenchError> {
        let payload = PublishPayload {
            topic: Bytes::copy_from_slice(topic.as_bytes()),
            qos,
            message,
            ttl_ms: None,
            partition_key: None,
        }
        .encode()?;
        let frame = Frame {
            msg_type: FrameType::Publish,
            correlation_id: self.next_correlation_id(),
            payload,
        };
        self.send_frame(&frame).await
    }

    /// Batched publish: encodes the PUBLISH frame into the per-client
    /// `out` buffer and only issues a TCP `write_all` once the buffer
    /// reaches `BATCH_FLUSH_BYTES`. Cuts syscalls per second by 1-2
    /// orders of magnitude on loopback small-payload publish loops,
    /// matching NATS's standard PUB-line pipelining.
    ///
    /// The caller MUST call [`Self::flush_publishes`] before any
    /// non-publish operation (subscribe, ack, drop) and at the end
    /// of a publish run, otherwise trailing frames stay buffered.
    pub async fn publish_batched(
        &mut self,
        topic: &str,
        qos: u8,
        message: Bytes,
    ) -> Result<(), BenchError> {
        let payload = PublishPayload {
            topic: Bytes::copy_from_slice(topic.as_bytes()),
            qos,
            message,
            ttl_ms: None,
            partition_key: None,
        }
        .encode()?;
        let frame = Frame {
            msg_type: FrameType::Publish,
            correlation_id: self.next_correlation_id(),
            payload,
        };
        // Encode directly into the per-client out buffer; no
        // intermediate BytesMut allocation per frame.
        encode_frame(&frame, &mut self.out)?;
        if self.out.len() >= BATCH_FLUSH_BYTES {
            self.flush_publishes().await?;
        }
        Ok(())
    }

    /// Flush any buffered batched publishes. Must be called before
    /// any operation that needs an immediate response (subscribe,
    /// ack, etc.) or before the connection is dropped.
    pub async fn flush_publishes(&mut self) -> Result<(), BenchError> {
        if self.out.is_empty() {
            return Ok(());
        }
        self.stream.write_all(&self.out).await?;
        self.out.clear();
        Ok(())
    }

    /// Wait for the next server-initiated `DELIVER` frame, decode it,
    /// and return the parsed payload. Returns `None` on timeout or
    /// stream EOF. The broker side of v2 always pushes via
    /// `DELIVER`; the legacy `POLL` path no longer applies because
    /// `handle_subscribe_slot` registers the subscription in
    /// shared-buffer push mode (no per-sub queue to poll).
    pub async fn next_delivery(
        &mut self,
        timeout_dur: Duration,
    ) -> Option<(Frame, DeliverPayload)> {
        loop {
            let resp = timeout(timeout_dur, self.recv_frame())
                .await
                .ok()
                .flatten()?;
            match resp.msg_type {
                FrameType::Deliver => {
                    let payload = DeliverPayload::decode(&resp.payload).ok()?;
                    return Some((resp, payload));
                }
                // Drain any inband Acks/Nacks (e.g. for SUBSCRIBE / PUBLISH
                // responses that arrive after the subscribe completed)
                // and keep waiting for the next DELIVER. The broker sends
                // ACK frames for SUBSCRIBE / AUTH / HELLO and NACK frames
                // on errors; any of these may interleave with DELIVERs on
                // a busy connection.
                FrameType::Ack | FrameType::Nack | FrameType::Pong => continue,
                _ => continue,
            }
        }
    }

    pub async fn ack(&mut self, sub_id: u64, delivery_tag: u64) -> Result<(), BenchError> {
        let payload = AckPayload {
            subscription_id: sub_id,
        }
        .encode()?;
        // The broker matches ACKs to inflight deliveries by the frame's
        // correlation_id, which the wire format sets to delivery_tag.
        let frame = Frame {
            msg_type: FrameType::Ack,
            correlation_id: delivery_tag,
            payload,
        };
        self.send_frame(&frame).await
    }
}

pub enum BrokerKind {
    NatsCore,
    NatsJetStream,
    BlipMqQos0,
    BlipMqQos1,
}

pub struct BenchmarkCase {
    pub broker: BrokerKind,
    pub subject: String,
    pub message_size: usize,
    pub publishers: usize,
    pub subscribers: usize,
    pub total_messages: u64,
    pub nats_url: String,
    pub addr: SocketAddr,
    pub api_key: String,
    pub monitor: Option<ResourceMonitor>,
}

pub async fn run_case(case: BenchmarkCase) -> Result<BenchResult, BenchError> {
    match case.broker {
        BrokerKind::NatsCore => run_nats(case, false).await,
        BrokerKind::NatsJetStream => run_nats(case, true).await,
        BrokerKind::BlipMqQos0 => run_bmq(case, 0).await,
        BrokerKind::BlipMqQos1 => run_bmq(case, 1).await,
    }
}

async fn run_bmq(case: BenchmarkCase, qos: u8) -> Result<BenchResult, BenchError> {
    let monitor = case
        .monitor
        .unwrap_or_else(|| ResourceMonitor::new(None, None, Duration::from_millis(500)));
    let start = Instant::now();
    let fut = async {
        let total_target = case.total_messages * case.subscribers as u64;
        let received = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let mut subscriber_handles = Vec::new();
        for _ in 0..case.subscribers {
            let addr = case.addr;
            let api_key = case.api_key.clone();
            let subject = case.subject.clone();
            let received = received.clone();
            let stop = stop.clone();
            let mut stats = LatencyStats::default();

            let handle = tokio::spawn(async move {
                let mut client = BmqClient::connect(addr, &api_key).await?;
                let sub_id = client.subscribe(&subject, qos).await?;

                while received.load(Ordering::Relaxed) < total_target {
                    if stop.load(Ordering::Relaxed)
                        && received.load(Ordering::Relaxed) >= total_target
                    {
                        break;
                    }

                    // v2 push: block on the next DELIVER frame. Short
                    // timeout so the harness can re-check the `stop`
                    // flag and bail out cleanly between scenarios.
                    if let Some((frame, payload)) =
                        client.next_delivery(Duration::from_millis(500)).await
                    {
                        if let Some(sent_ns) = extract_timestamp_ns(&payload.message) {
                            let latency = now_ns().saturating_sub(sent_ns);
                            stats.record_ns(latency);
                        }
                        received.fetch_add(1, Ordering::Relaxed);
                        if qos == 1 {
                            let _ = client.ack(sub_id, frame.correlation_id).await;
                        }
                    }
                }

                Ok::<LatencyStats, BenchError>(stats)
            });

            subscriber_handles.push(handle);
        }

        let mut publisher_handles = Vec::new();
        let counter = Arc::new(AtomicU64::new(0));
        for _ in 0..case.publishers {
            let addr = case.addr;
            let api_key = case.api_key.clone();
            let subject = case.subject.clone();
            let message_size = case.message_size;
            let total = case.total_messages;
            let counter = counter.clone();

            let handle = tokio::spawn(async move {
                let mut client = BmqClient::connect(addr, &api_key).await?;
                loop {
                    let idx = counter.fetch_add(1, Ordering::Relaxed);
                    if idx >= total {
                        break;
                    }
                    let payload = build_payload(message_size);
                    client.publish(&subject, qos, payload).await?;
                }
                Ok::<(), BenchError>(())
            });
            publisher_handles.push(handle);
        }

        for handle in publisher_handles {
            handle
                .await
                .map_err(|e| format!("bmq publisher join error: {e}"))??;
        }
        stop.store(true, Ordering::SeqCst);

        let mut combined = LatencyStats::default();
        for handle in subscriber_handles {
            let stats = handle
                .await
                .map_err(|e| format!("bmq subscriber join error: {e}"))??;
            combined.merge(stats);
        }

        let elapsed = start.elapsed();
        let deliveries = received.load(Ordering::SeqCst);
        let throughput = if elapsed.as_secs_f64() > 0.0 {
            deliveries as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };
        let lat = combined.summarize();

        Ok::<BenchResult, BenchError>(BenchResult {
            broker: "blipmq".to_string(),
            qos: format!("qos{qos}"),
            message_size: case.message_size,
            publishers: case.publishers,
            subscribers: case.subscribers,
            total_messages: case.total_messages,
            throughput_mps: throughput,
            latency_p50_us: lat.p50_us,
            latency_p95_us: lat.p95_us,
            latency_p99_us: lat.p99_us,
            cpu_percent: 0.0,
            memory_bytes: 0,
        })
    };

    let (result, usage) = monitor.measure(fut).await?;
    Ok(BenchResult {
        cpu_percent: usage.avg_cpu_percent,
        memory_bytes: usage.max_memory_bytes,
        ..result
    })
}

async fn run_nats(case: BenchmarkCase, jetstream: bool) -> Result<BenchResult, BenchError> {
    let monitor = case
        .monitor
        .unwrap_or_else(|| ResourceMonitor::new(None, None, Duration::from_millis(500)));
    let start = Instant::now();
    let url = case.nats_url.clone();
    let subject = case.subject.clone();
    let message_size = case.message_size;
    let publishers = case.publishers;
    let subscribers = case.subscribers;
    let total_messages = case.total_messages;
    let qos_label = if jetstream { "jetstream" } else { "core" };
    let fut = async move {
        let total_target = total_messages * subscribers as u64;
        let received = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let mut subscriber_handles = Vec::new();

        for _ in 0..subscribers {
            let url = url.clone();
            let subject = subject.clone();
            let received = received.clone();
            let stop = stop.clone();

            let handle = if jetstream {
                tokio::task::spawn_blocking(move || {
                    let nc = nats::connect(&url)?;
                    let js = nats::jetstream::new(nc);
                    let sub = js.subscribe(&subject)?;
                    let mut stats = LatencyStats::default();
                    while received.load(Ordering::Relaxed) < total_target {
                        if stop.load(Ordering::Relaxed)
                            && received.load(Ordering::Relaxed) >= total_target
                        {
                            break;
                        }
                        match sub.next_timeout(Duration::from_millis(200)) {
                            Ok(msg) => {
                                if let Some(sent_ns) = extract_timestamp_ns(&msg.data) {
                                    let latency = now_ns().saturating_sub(sent_ns);
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
                    let nc = nats::asynk::connect(&url).await?;
                    let sub = nc.subscribe(&subject).await?;
                    let mut stats = LatencyStats::default();
                    while received.load(Ordering::Relaxed) < total_target {
                        if stop.load(Ordering::Relaxed)
                            && received.load(Ordering::Relaxed) >= total_target
                        {
                            break;
                        }
                        match tokio::time::timeout(Duration::from_millis(200), sub.next()).await {
                            Ok(Some(msg)) => {
                                if let Some(sent_ns) = extract_timestamp_ns(&msg.data) {
                                    let latency = now_ns().saturating_sub(sent_ns);
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

            subscriber_handles.push(handle);
        }

        let counter = Arc::new(AtomicU64::new(0));
        let mut publisher_handles = Vec::new();
        for _ in 0..publishers {
            let url = url.clone();
            let subject = subject.clone();
            let total_messages = total_messages;
            let counter = counter.clone();
            let message_size = message_size;

            let handle = if jetstream {
                tokio::task::spawn_blocking(move || {
                    let nc = nats::connect(&url)?;
                    let js = nats::jetstream::new(nc);
                    loop {
                        let idx = counter.fetch_add(1, Ordering::Relaxed);
                        if idx >= total_messages {
                            break;
                        }
                        let payload = build_payload(message_size);
                        js.publish(&subject, payload)?;
                    }
                    Ok::<(), BenchError>(())
                })
            } else {
                tokio::spawn(async move {
                    let nc = nats::asynk::connect(&url).await?;
                    loop {
                        let idx = counter.fetch_add(1, Ordering::Relaxed);
                        if idx >= total_messages {
                            break;
                        }
                        let payload = build_payload(message_size);
                        nats::asynk::Connection::publish(&nc, &subject, payload).await?;
                    }
                    nc.flush().await?;
                    Ok::<(), BenchError>(())
                })
            };
            publisher_handles.push(handle);
        }

        for handle in publisher_handles {
            handle
                .await
                .map_err(|e| format!("nats publisher join error: {e}"))??;
        }
        stop.store(true, Ordering::SeqCst);

        let mut combined = LatencyStats::default();
        for handle in subscriber_handles {
            let stats = handle
                .await
                .map_err(|e| format!("nats subscriber join error: {e}"))??;
            combined.merge(stats);
        }

        let elapsed = start.elapsed();
        let deliveries = received.load(Ordering::SeqCst);
        let throughput = if elapsed.as_secs_f64() > 0.0 {
            deliveries as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };
        let lat = combined.summarize();

        Ok::<BenchResult, BenchError>(BenchResult {
            broker: "nats".to_string(),
            qos: qos_label.to_string(),
            message_size,
            publishers,
            subscribers,
            total_messages,
            throughput_mps: throughput,
            latency_p50_us: lat.p50_us,
            latency_p95_us: lat.p95_us,
            latency_p99_us: lat.p99_us,
            cpu_percent: 0.0,
            memory_bytes: 0,
        })
    };

    let (result, usage) = monitor.measure(fut).await?;
    Ok(BenchResult {
        cpu_percent: usage.avg_cpu_percent,
        memory_bytes: usage.max_memory_bytes,
        ..result
    })
}

pub fn write_results_csv(
    path: &std::path::Path,
    results: &[BenchResult],
) -> Result<(), BenchError> {
    let mut writer = csv::Writer::from_path(path)?;
    for r in results {
        writer.serialize(r)?;
    }
    writer.flush()?;
    Ok(())
}

pub fn write_results_json(
    path: &std::path::Path,
    results: &[BenchResult],
) -> Result<(), BenchError> {
    let file = std::fs::File::create(path)?;
    serde_json::to_writer_pretty(file, results)?;
    Ok(())
}
