//! End-to-end smoke + scale tests for v2 push delivery.
//!
//! These tests stand up a real `Server` over a loopback TCP listener, run
//! a number of clients through HELLO/AUTH/SUBSCRIBE, publish messages
//! through one client, and verify that DELIVER frames arrive on the
//! subscribers — exercising the new reader/writer task split, the per-conn
//! push channel, and `Broker::subscribe_with_conn`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use corelib::{Broker, BrokerConfig, QoSLevel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::time::timeout;

use auth::StaticApiKeyValidator;
use net::{
    encode_frame, try_decode_frame, AckPayload, AuthPayload, BrokerHandler, DeliverPayload, Frame,
    FrameType, HelloPayload, NetworkConfig, PublishPayload, Server, SubscribePayload,
    PROTOCOL_VERSION,
};

const API_KEY: &str = "test-key";
const TEST_TIMEOUT: Duration = Duration::from_secs(5);

async fn start_test_server() -> (SocketAddr, watch::Sender<bool>, Arc<Broker>) {
    let broker = Arc::new(Broker::new(BrokerConfig {
        default_qos: QoSLevel::AtMostOnce,
        message_ttl: Duration::from_secs(60),
        per_subscriber_queue_capacity: 8192,
        max_retries: 3,
        retry_base_delay: Duration::from_millis(50),
        ..Default::default()
    }));

    let handler = BrokerHandler::new(broker.clone());
    let auth = Arc::new(StaticApiKeyValidator::from_keys([API_KEY.to_string()]));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Bind to ephemeral port; capture it via TcpListener::bind here so the
    // test knows where to connect before Server::start runs.
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let probe = tokio::net::TcpListener::bind(bind).await.unwrap();
    let local = probe.local_addr().unwrap();
    drop(probe); // release port; Server will rebind. Tiny race window;
                 // acceptable for a smoke test.

    let server = Server::new(
        NetworkConfig { bind_addr: local, tls: None },
        handler,
        auth,
        shutdown_rx,
    );

    tokio::spawn(async move {
        let _ = server.start().await;
    });

    // Give the server a moment to bind.
    tokio::time::sleep(Duration::from_millis(50)).await;

    (local, shutdown_tx, broker)
}

struct Client {
    stream: TcpStream,
    read_buf: BytesMut,
    next_corr: u64,
}

impl Client {
    async fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).await.expect("connect");
        stream.set_nodelay(true).unwrap();
        Self {
            stream,
            read_buf: BytesMut::with_capacity(8 * 1024),
            next_corr: 1,
        }
    }

    fn next_corr(&mut self) -> u64 {
        let v = self.next_corr;
        self.next_corr = self.next_corr.wrapping_add(1);
        v
    }

    async fn write_frame(&mut self, frame: Frame) {
        let mut buf = BytesMut::new();
        encode_frame(&frame, &mut buf).unwrap();
        self.stream.write_all(&buf).await.unwrap();
    }

    async fn read_one_frame(&mut self) -> Frame {
        loop {
            if let Some(f) = try_decode_frame(&mut self.read_buf).unwrap() {
                return f;
            }
            let n = self.stream.read_buf(&mut self.read_buf).await.unwrap();
            if n == 0 {
                panic!("connection closed before frame arrived");
            }
        }
    }

    async fn handshake(&mut self) {
        // HELLO
        let corr = self.next_corr();
        self.write_frame(Frame {
            msg_type: FrameType::Hello,
            correlation_id: corr,
            payload: HelloPayload {
                protocol_version: PROTOCOL_VERSION,
            }
            .encode(),
        })
        .await;
        let resp = self.read_one_frame().await;
        assert_eq!(resp.msg_type, FrameType::Ack, "HELLO must be ACKed");

        // AUTH
        let corr = self.next_corr();
        self.write_frame(Frame {
            msg_type: FrameType::Auth,
            correlation_id: corr,
            payload: AuthPayload {
                api_key: API_KEY.to_string(),
            }
            .encode()
            .unwrap(),
        })
        .await;
        let resp = self.read_one_frame().await;
        assert_eq!(resp.msg_type, FrameType::Ack, "AUTH must be ACKed");
    }

    async fn subscribe(&mut self, topic: &str, qos: u8) -> u64 {
        let corr = self.next_corr();
        self.write_frame(Frame {
            msg_type: FrameType::Subscribe,
            correlation_id: corr,
            payload: SubscribePayload {
                topic: topic.to_string(),
                qos,
            }
            .encode()
            .unwrap(),
        })
        .await;
        let resp = self.read_one_frame().await;
        assert_eq!(resp.msg_type, FrameType::Ack);
        AckPayload::decode(&resp.payload).unwrap().subscription_id
    }

    async fn publish(&mut self, topic: &str, qos: u8, message: Bytes) {
        let corr = self.next_corr();
        self.write_frame(Frame {
            msg_type: FrameType::Publish,
            correlation_id: corr,
            payload: PublishPayload {
                topic: Bytes::copy_from_slice(topic.as_bytes()),
                qos,
                message,
            }
            .encode()
            .unwrap(),
        })
        .await;
    }
}

#[tokio::test]
async fn push_delivery_single_pub_single_sub() {
    let (addr, _shutdown, _broker) = start_test_server().await;

    let mut sub = Client::connect(addr).await;
    sub.handshake().await;
    let _sub_id = sub.subscribe("smoke", 0).await;

    let mut pub_client = Client::connect(addr).await;
    pub_client.handshake().await;
    pub_client
        .publish("smoke", 0, Bytes::from_static(b"hello-push"))
        .await;

    let frame = timeout(TEST_TIMEOUT, sub.read_one_frame())
        .await
        .expect("DELIVER frame should arrive within timeout");
    assert_eq!(frame.msg_type, FrameType::Deliver);
    let payload = DeliverPayload::decode(&frame.payload).unwrap();
    assert_eq!(payload.topic, "smoke");
    assert_eq!(payload.qos, 0);
    assert_eq!(payload.delivery_tag, 0); // QoS0 → no tag
    assert_eq!(payload.message.as_ref(), b"hello-push");
}

#[tokio::test]
async fn push_delivery_qos1_carries_tag() {
    let (addr, _shutdown, _broker) = start_test_server().await;

    let mut sub = Client::connect(addr).await;
    sub.handshake().await;
    let _sub_id = sub.subscribe("q1", 1).await;

    let mut pub_client = Client::connect(addr).await;
    pub_client.handshake().await;
    pub_client
        .publish("q1", 1, Bytes::from_static(b"durable"))
        .await;

    let frame = timeout(TEST_TIMEOUT, sub.read_one_frame())
        .await
        .expect("DELIVER frame should arrive");
    assert_eq!(frame.msg_type, FrameType::Deliver);
    let payload = DeliverPayload::decode(&frame.payload).unwrap();
    assert_eq!(payload.qos, 1);
    assert!(payload.delivery_tag != 0, "QoS1 must carry a tag");
}

#[tokio::test]
async fn push_delivery_batches_many_messages() {
    let (addr, _shutdown, _broker) = start_test_server().await;

    let mut sub = Client::connect(addr).await;
    sub.handshake().await;
    sub.subscribe("burst", 0).await;

    let mut pub_client = Client::connect(addr).await;
    pub_client.handshake().await;

    const N: usize = 500;
    for i in 0..N {
        pub_client
            .publish("burst", 0, Bytes::from(format!("msg-{i}").into_bytes()))
            .await;
    }

    let recv = async {
        let mut got = 0usize;
        while got < N {
            let frame = sub.read_one_frame().await;
            assert_eq!(frame.msg_type, FrameType::Deliver);
            let payload = DeliverPayload::decode(&frame.payload).unwrap();
            let expected = format!("msg-{got}");
            assert_eq!(payload.message.as_ref(), expected.as_bytes());
            got += 1;
        }
        got
    };

    let got = timeout(Duration::from_secs(15), recv)
        .await
        .expect("receive completes within timeout");
    assert_eq!(got, N);
}

#[tokio::test]
async fn push_delivery_fans_out_to_many_subs() {
    let (addr, _shutdown, _broker) = start_test_server().await;

    // 32 subscribers (1k would dominate test suite latency; we still want
    // a fanout test to guard against regressions in the multi-sub path).
    const NUM_SUBS: usize = 32;
    const NUM_MSGS: usize = 50;

    let mut subs = Vec::with_capacity(NUM_SUBS);
    for _ in 0..NUM_SUBS {
        let mut c = Client::connect(addr).await;
        c.handshake().await;
        c.subscribe("fan", 0).await;
        subs.push(c);
    }

    let mut pub_client = Client::connect(addr).await;
    pub_client.handshake().await;

    for i in 0..NUM_MSGS {
        pub_client
            .publish("fan", 0, Bytes::from(format!("m{i}").into_bytes()))
            .await;
    }

    // Drain each subscriber.
    let drain = async {
        for (idx, sub) in subs.iter_mut().enumerate() {
            for i in 0..NUM_MSGS {
                let frame = sub.read_one_frame().await;
                assert_eq!(frame.msg_type, FrameType::Deliver, "sub {idx} msg {i}");
                let payload = DeliverPayload::decode(&frame.payload).unwrap();
                let expected = format!("m{i}");
                assert_eq!(
                    payload.message.as_ref(),
                    expected.as_bytes(),
                    "sub {idx} expected {expected} at index {i}"
                );
            }
        }
    };

    timeout(Duration::from_secs(15), drain)
        .await
        .expect("all subs receive all messages within timeout");
}
