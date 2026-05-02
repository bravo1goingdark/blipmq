//! M2 wire benchmark: 1 publisher × N subscribers fanout delivery rate.
//!
//! Measures total deliveries/sec (sum across all subscribers) on real TCP.
//! Single publisher pushes M messages to a topic; N subscribers (each on
//! its own TCP connection) drain DELIVER frames concurrently. Throughput
//! reported is `(M × N) / wall_time` — what NATS quotes as its pub-sub
//! headline.

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
const TOPIC: &str = "fan";

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

struct FanoutResult {
    publish_elapsed: Duration,
    delivery_elapsed: Duration,
    total_publishes: u64,
    total_deliveries: u64,
    drops: u64,
}

async fn run_fanout(
    addr: SocketAddr,
    broker: Arc<Broker>,
    n_subs: usize,
    n_pubs: u64,
    payload_len: usize,
) -> FanoutResult {
    // Bring up subscribers.
    let mut subs = Vec::with_capacity(n_subs);
    for _ in 0..n_subs {
        let mut sub = TcpStream::connect(addr).await.unwrap();
        sub.set_nodelay(true).unwrap();
        let mut rb = BytesMut::with_capacity(64 * 1024);
        handshake(&mut sub, &mut rb).await;
        subscribe(&mut sub, &mut rb, TOPIC).await;
        subs.push((sub, rb));
    }

    // Spawn a drainer per subscriber.
    let drain_start = Instant::now();
    let mut drainers = Vec::with_capacity(n_subs);
    for (mut stream, mut rb) in subs {
        let target = n_pubs;
        let jh = tokio::spawn(async move {
            let mut got: u64 = 0;
            loop {
                while let Some(f) = try_decode_frame(&mut rb).unwrap() {
                    if f.msg_type == FrameType::Deliver {
                        got += 1;
                    }
                }
                if got >= target {
                    break;
                }
                match tokio::time::timeout(Duration::from_secs(2), stream.read_buf(&mut rb)).await {
                    Ok(Ok(0)) => break,
                    Ok(Ok(_)) => {}
                    Ok(Err(_)) => break,
                    Err(_) => break,
                }
            }
            got
        });
        drainers.push(jh);
    }

    // Publisher.
    let mut pub_stream = TcpStream::connect(addr).await.unwrap();
    pub_stream.set_nodelay(true).unwrap();
    let mut pub_buf = BytesMut::with_capacity(64 * 1024);
    handshake(&mut pub_stream, &mut pub_buf).await;

    let payload = Bytes::from(vec![0xABu8; payload_len]);
    let publish_payload_bytes = PublishPayload {
        topic: Bytes::copy_from_slice(TOPIC.as_bytes()),
        qos: 0,
        message: payload,
        ttl_ms: None,
    }
    .encode()
    .unwrap();

    const COALESCE: u64 = 256;
    let mut wire =
        BytesMut::with_capacity((publish_payload_bytes.len() + 16) as usize * COALESCE as usize);

    let drops_before = broker.push_dropped_total();
    let pub_start = Instant::now();
    let mut sent: u64 = 0;
    while sent < n_pubs {
        wire.clear();
        let batch = COALESCE.min(n_pubs - sent);
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
    let publish_elapsed = pub_start.elapsed();

    let mut total_deliveries: u64 = 0;
    for d in drainers {
        total_deliveries += d.await.unwrap();
    }
    let delivery_elapsed = drain_start.elapsed();
    let drops = broker.push_dropped_total() - drops_before;

    FanoutResult {
        publish_elapsed,
        delivery_elapsed,
        total_publishes: sent,
        total_deliveries,
        drops,
    }
}

fn main() {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let (addr, _shutdown, broker) = start_server().await;

        // Warm up.
        let _ = run_fanout(addr, broker.clone(), 4, 1_000, 64).await;

        println!(
            "{:<24} {:>6} {:>10} {:>10} {:>10} {:>13} {:>13}",
            "case", "subs", "pubs", "delivs", "drops", "pub/s", "deliv/s",
        );

        for &(n_subs, n_pubs, payload, label) in &[
            (8usize, 100_000u64, 64usize, "8 subs x 100K x 64B"),
            (32usize, 50_000u64, 64usize, "32 subs x 50K x 64B"),
            (64usize, 50_000u64, 64usize, "64 subs x 50K x 64B"),
            (128usize, 25_000u64, 64usize, "128 subs x 25K x 64B"),
            (256usize, 10_000u64, 64usize, "256 subs x 10K x 64B"),
            (8usize, 100_000u64, 256usize, "8 subs x 100K x 256B"),
            (32usize, 50_000u64, 256usize, "32 subs x 50K x 256B"),
        ] {
            let r = run_fanout(addr, broker.clone(), n_subs, n_pubs, payload).await;
            let pub_rate = r.total_publishes as f64 / r.publish_elapsed.as_secs_f64();
            let deliv_rate = r.total_deliveries as f64 / r.delivery_elapsed.as_secs_f64();
            println!(
                "{label:<24} {n_subs:>6} {pubs:>10} {delivs:>10} {drops:>10} {pub_rate:>13.0} {deliv_rate:>13.0}",
                pubs = r.total_publishes,
                delivs = r.total_deliveries,
                drops = r.drops,
            );
        }
    });
}
