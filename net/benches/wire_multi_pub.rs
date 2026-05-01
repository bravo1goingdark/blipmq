//! Multi-pipe wire benchmark — measures broker-side scaling.
//!
//! Spins up N independent (publisher, subscriber) pairs, each on its own
//! topic. Each pair is a separate TCP-pipe-to-TCP-pipe path through the
//! broker; nothing is shared between pairs at the broker level except the
//! sharded subscription map and the topic registry. Aggregate ingress
//! and aggregate delivery should both scale near-linearly with N until
//! some shared-resource ceiling is hit (CPU cores, the runtime
//! scheduler, the subscription shard map).
//!
//! This bench is the right shape to interpret M1: "single-pipe 5 M
//! msg/s" requires either a single TCP conn moving 5 M frames/s (which
//! is at the edge of userspace TCP on one core) or, more realistically,
//! N parallel pipes aggregating to >= 5 M.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use corelib::{Broker, BrokerConfig, QoSLevel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use auth::StaticApiKeyValidator;
use net::{
    encode_frame, try_decode_frame, AckPayload, AuthPayload, BrokerHandler, Frame, FrameType,
    HelloPayload, NetworkConfig, PublishPayload, Server, SubscribePayload, PROTOCOL_VERSION,
};

const API_KEY: &str = "bench";

async fn start_server() -> (SocketAddr, watch::Sender<bool>, Arc<Broker>) {
    let broker = Arc::new(Broker::new(BrokerConfig {
        default_qos: QoSLevel::AtMostOnce,
        message_ttl: Duration::from_secs(60),
        per_subscriber_queue_capacity: 65_536,
        max_retries: 3,
        retry_base_delay: Duration::from_millis(50),
    }));

    let handler = BrokerHandler::new(broker.clone());
    let auth = Arc::new(StaticApiKeyValidator::from_keys([API_KEY.to_string()]));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = probe.local_addr().unwrap();
    drop(probe);

    let server = Server::new(
        NetworkConfig { bind_addr: local },
        handler,
        auth,
        shutdown_rx,
    );

    tokio::spawn(async move {
        let _ = server.start().await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
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

/// One independent pipe: (publisher conn, subscriber conn) on a unique
/// topic, exchanging `n_msgs` of `payload_len` bytes. Returns the wall
/// time from "publish loop start" to "subscriber drained `n_msgs`".
async fn run_pipe(addr: SocketAddr, topic: String, n_msgs: u64, payload_len: usize) -> Duration {
    // Subscriber.
    let mut sub = TcpStream::connect(addr).await.unwrap();
    sub.set_nodelay(true).unwrap();
    let mut sub_buf = BytesMut::with_capacity(64 * 1024);
    handshake(&mut sub, &mut sub_buf).await;
    subscribe(&mut sub, &mut sub_buf, &topic).await;

    // Drainer.
    let drain_jh: JoinHandle<u64> = tokio::spawn(async move {
        let mut got: u64 = 0;
        loop {
            while let Some(f) = try_decode_frame(&mut sub_buf).unwrap() {
                if f.msg_type == FrameType::Deliver {
                    got += 1;
                }
            }
            if got >= n_msgs {
                break;
            }
            match tokio::time::timeout(Duration::from_secs(5), sub.read_buf(&mut sub_buf)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(_)) => {}
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        got
    });

    // Publisher.
    let mut pub_stream = TcpStream::connect(addr).await.unwrap();
    pub_stream.set_nodelay(true).unwrap();
    let mut pub_buf = BytesMut::with_capacity(64 * 1024);
    handshake(&mut pub_stream, &mut pub_buf).await;

    let payload = Bytes::from(vec![0xABu8; payload_len]);
    let publish_payload_bytes = PublishPayload {
        topic: Bytes::copy_from_slice(topic.as_bytes()),
        qos: 0,
        message: payload,
    }
    .encode()
    .unwrap();

    const COALESCE: u64 = 256;
    let mut wire = BytesMut::with_capacity(
        (publish_payload_bytes.len() + 16) as usize * COALESCE as usize,
    );

    let start = Instant::now();
    let mut sent: u64 = 0;
    while sent < n_msgs {
        wire.clear();
        let batch = COALESCE.min(n_msgs - sent);
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
    let _received = drain_jh.await.unwrap();
    start.elapsed()
}

fn main() {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let (addr, _shutdown, _broker) = start_server().await;

        // Warm up.
        let _ = run_pipe(addr, "warmup".to_string(), 5_000, 64).await;

        println!(
            "{:<24} {:>5} {:>10} {:>10} {:>13} {:>13}",
            "case", "pipes", "per_pipe", "total", "agg_ingress/s", "agg_deliv/s"
        );

        // N independent (pub, sub) pipes, each on its own topic.
        // Reported numbers are aggregates: total messages / max wall time
        // across the N pipes (since they're concurrent).
        let cases: &[(usize, u64, usize, &str)] = &[
            (1, 500_000, 64, "1 pipe x 500K x 64B"),
            (2, 250_000, 64, "2 pipes x 250K x 64B"),
            (4, 125_000, 64, "4 pipes x 125K x 64B"),
            (8, 62_500, 64, "8 pipes x 62.5K x 64B"),
            (16, 31_250, 64, "16 pipes x 31.25K x 64B"),
        ];

        for &(n_pipes, per_pipe, payload, label) in cases {
            let total = (n_pipes as u64) * per_pipe;

            let mut handles: Vec<JoinHandle<Duration>> = Vec::with_capacity(n_pipes);
            let start = Instant::now();
            for i in 0..n_pipes {
                let topic = format!("pipe-{i}");
                handles.push(tokio::spawn(run_pipe(addr, topic, per_pipe, payload)));
            }
            let mut max_pipe = Duration::from_secs(0);
            for h in handles {
                let d = h.await.unwrap();
                if d > max_pipe {
                    max_pipe = d;
                }
            }
            let wall = start.elapsed();

            // Aggregate throughput uses wall time from "first pipe started"
            // to "last pipe finished" so the figure reflects what the
            // broker actually sustained at peak concurrency.
            let agg = total as f64 / wall.as_secs_f64();
            println!(
                "{label:<24} {n_pipes:>5} {per_pipe:>10} {total:>10} {agg:>13.0} {agg2:>13.0}",
                agg2 = total as f64 / max_pipe.as_secs_f64(),
            );
        }
    });
}
