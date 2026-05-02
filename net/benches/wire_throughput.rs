//! Over-the-wire throughput micro-benchmark.
//!
//! Spins up a real `Server` and a TCP client publisher + a TCP client
//! subscriber. Measures **ingress** msg/s — i.e. number of PUBLISH frames
//! the broker can absorb from one publisher while one subscriber is
//! draining DELIVER frames. This is the M1 milestone shape.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use corelib::{Broker, BrokerConfig, QoSLevel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio::sync::watch;

use auth::StaticApiKeyValidator;
use net::{
    encode_frame, try_decode_frame, AckPayload, AuthPayload, BrokerHandler, Frame, FrameType,
    HelloPayload, NetworkConfig, PublishPayload, Server, SubscribePayload, PROTOCOL_VERSION,
};

const API_KEY: &str = "bench";
const TOPIC: &str = "bench";

async fn start_server() -> (SocketAddr, watch::Sender<bool>, Arc<Broker>) {
    let broker = Arc::new(Broker::new(BrokerConfig {
        default_qos: QoSLevel::AtMostOnce,
        message_ttl: Duration::from_secs(60),
        per_subscriber_queue_capacity: 65_536,
        max_retries: 3,
        retry_base_delay: Duration::from_millis(50),
        ..Default::default()
    }));

    let handler = BrokerHandler::new(broker.clone());
    let auth = Arc::new(StaticApiKeyValidator::from_keys([API_KEY.to_string()]));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = probe.local_addr().unwrap();
    drop(probe);

    let server = Server::new(
        NetworkConfig {
            bind_addr: local,
            tls: None,
        },
        handler,
        auth,
        shutdown_rx,
    );

    tokio::spawn(async move {
        let _ = server.start().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (local, shutdown_tx, broker)
}

async fn handshake(stream: &mut TcpStream, read_buf: &mut BytesMut) {
    // HELLO
    let mut buf = BytesMut::new();
    encode_frame(
        &Frame {
            msg_type: FrameType::Hello,
            correlation_id: 1,
            payload: HelloPayload {
                protocol_version: PROTOCOL_VERSION,
            }
            .encode(),
        },
        &mut buf,
    )
    .unwrap();
    stream.write_all(&buf).await.unwrap();
    read_until_frame(stream, read_buf).await;

    // AUTH
    buf.clear();
    encode_frame(
        &Frame {
            msg_type: FrameType::Auth,
            correlation_id: 2,
            payload: AuthPayload {
                api_key: API_KEY.to_string(),
            }
            .encode()
            .unwrap(),
        },
        &mut buf,
    )
    .unwrap();
    stream.write_all(&buf).await.unwrap();
    read_until_frame(stream, read_buf).await;
}

async fn read_until_frame(stream: &mut TcpStream, read_buf: &mut BytesMut) -> Frame {
    loop {
        if let Some(f) = try_decode_frame(read_buf).unwrap() {
            return f;
        }
        let n = stream.read_buf(read_buf).await.unwrap();
        if n == 0 {
            panic!("connection closed");
        }
    }
}

async fn subscribe(stream: &mut TcpStream, read_buf: &mut BytesMut, topic: &str) -> u64 {
    let mut buf = BytesMut::new();
    encode_frame(
        &Frame {
            msg_type: FrameType::Subscribe,
            correlation_id: 3,
            payload: SubscribePayload {
                topic: topic.to_string(),
                qos: 0,
                group: None,
            }
            .encode()
            .unwrap(),
        },
        &mut buf,
    )
    .unwrap();
    stream.write_all(&buf).await.unwrap();
    let resp = read_until_frame(stream, read_buf).await;
    AckPayload::decode(&resp.payload).unwrap().subscription_id
}

struct IngressResult {
    publish_elapsed: Duration,
    delivery_elapsed: Duration,
    sent: u64,
    received: u64,
}

/// Run one ingress measurement: publish `n` frames of `payload_len` bytes
/// from one client; a subscriber is connected and draining concurrently.
/// Returns both the publish-loop wall time (broker ingestion ceiling) and
/// the wall time until the subscriber has drained all received DELIVER
/// frames (end-to-end ceiling). If drops occur, `received < sent`.
async fn run_ingress(addr: SocketAddr, n: u64, payload_len: usize) -> IngressResult {
    // Subscriber.
    let mut sub = TcpStream::connect(addr).await.unwrap();
    sub.set_nodelay(true).unwrap();
    let mut sub_buf = BytesMut::with_capacity(64 * 1024);
    handshake(&mut sub, &mut sub_buf).await;
    subscribe(&mut sub, &mut sub_buf, TOPIC).await;

    // Spawn drainer. Records the time it takes to receive each batch and
    // exits when (a) it has received `n` frames OR (b) idle for >2 seconds
    // (used to detect "publisher done, no more inbound" — a coarse sentinel
    // since the broker doesn't signal end-of-stream).
    let drain_start = Instant::now();
    let drain_jh = tokio::spawn(async move {
        let mut received: u64 = 0;
        let mut last_progress = Instant::now();
        loop {
            // Decode whatever's already buffered first.
            while let Some(f) = try_decode_frame(&mut sub_buf).unwrap() {
                if f.msg_type == FrameType::Deliver {
                    received += 1;
                    last_progress = Instant::now();
                }
            }
            if received >= n {
                break;
            }
            let timeout_dur = Duration::from_millis(500);
            match tokio::time::timeout(timeout_dur, sub.read_buf(&mut sub_buf)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(_)) => {}
                Ok(Err(_)) => break,
                Err(_) => {
                    if last_progress.elapsed() >= Duration::from_secs(2) {
                        break;
                    }
                }
            }
        }
        (received, drain_start.elapsed())
    });

    // Publisher.
    let mut pub_stream = TcpStream::connect(addr).await.unwrap();
    pub_stream.set_nodelay(true).unwrap();
    let mut pub_buf = BytesMut::with_capacity(64 * 1024);
    handshake(&mut pub_stream, &mut pub_buf).await;

    let payload = Bytes::from(vec![0xABu8; payload_len]);
    // Pre-encode each PUBLISH frame to remove allocator pressure from the
    // tight loop (we want to measure the broker's ingestion, not per-frame
    // allocation).
    let publish_payload_bytes = PublishPayload {
        topic: Bytes::copy_from_slice(TOPIC.as_bytes()),
        qos: 0,
        message: payload.clone(),
        ttl_ms: None,
    }
    .encode()
    .unwrap();

    // Coalesce multiple PUBLISH frames into one write to push beyond
    // syscall ceilings. The broker will decode and dispatch them sequentially.
    const COALESCE: u64 = 256;
    let mut wire =
        BytesMut::with_capacity((publish_payload_bytes.len() + 16) as usize * COALESCE as usize);

    let start = Instant::now();
    let mut sent: u64 = 0;
    while sent < n {
        wire.clear();
        let batch = COALESCE.min(n - sent);
        for _ in 0..batch {
            encode_frame(
                &Frame {
                    msg_type: FrameType::Publish,
                    correlation_id: sent,
                    payload: publish_payload_bytes.clone(),
                },
                &mut wire,
            )
            .unwrap();
            sent += 1;
        }
        pub_stream.write_all(&wire).await.unwrap();
    }
    pub_stream.flush().await.unwrap();
    let publish_elapsed = start.elapsed();

    // Wait for subscriber to drain (or its quiescence sentinel).
    let (received, delivery_elapsed) = tokio::time::timeout(Duration::from_secs(120), drain_jh)
        .await
        .expect("drain hard timeout")
        .unwrap();

    IngressResult {
        publish_elapsed,
        delivery_elapsed,
        sent,
        received,
    }
}

fn main() {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let (addr, _shutdown, _broker) = start_server().await;

        // Warm up.
        let _ = run_ingress(addr, 10_000, 64).await;

        println!(
            "{:<16} {:>10} {:>10} {:>10} {:>13} {:>13} {:>10}",
            "case", "sent", "recv", "drops", "pub_msg/s", "deliv_msg/s", "MiB/s"
        );

        for &(n, sz, label) in &[
            (100_000u64, 64usize, "100K x 64B"),
            (500_000u64, 64usize, "500K x 64B"),
            (1_000_000u64, 64usize, "1M x 64B"),
            (500_000u64, 256usize, "500K x 256B"),
            (200_000u64, 1024usize, "200K x 1KiB"),
        ] {
            let before_drops = _broker.push_dropped_total();
            let r = run_ingress(addr, n, sz).await;
            let drops = _broker.push_dropped_total() - before_drops;
            let pub_rate = r.sent as f64 / r.publish_elapsed.as_secs_f64();
            let deliv_rate = r.received as f64 / r.delivery_elapsed.as_secs_f64();
            let mb_s =
                (r.received as f64 * sz as f64) / r.delivery_elapsed.as_secs_f64() / (1024.0 * 1024.0);
            println!(
                "{label:<16} {sent:>10} {received:>10} {drops:>10} {pub_rate:>13.0} {deliv_rate:>13.0} {mb_s:>10.1}",
                sent = r.sent,
                received = r.received,
            );
        }
    });
}
