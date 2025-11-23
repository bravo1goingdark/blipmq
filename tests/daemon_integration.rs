use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use auth::StaticApiKeyValidator;
use corelib::{Broker, BrokerConfig, QoSLevel, TopicName};
use net::{
    AckPayload, AuthPayload, BrokerHandler, Frame, FrameResponse, FrameType,
    HelloPayload, MessageHandler, NetworkConfig, PollPayload, PublishPayload,
    Server, SubscribePayload, PROTOCOL_VERSION, encode_frame, try_decode_frame,
};
use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::time;

fn test_broker() -> Arc<Broker> {
    Arc::new(Broker::new(BrokerConfig {
        default_qos: QoSLevel::AtLeastOnce,
        message_ttl: Duration::from_secs(5),
        per_subscriber_queue_capacity: 1024,
        max_retries: 3,
        retry_base_delay: Duration::from_millis(20),
    }))
}

async fn start_test_server(
    broker: Arc<Broker>,
) -> (SocketAddr, watch::Sender<bool>) {
    // Bind to an ephemeral port and reuse that port for the server.
    let base_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let listener = tokio::net::TcpListener::bind(base_addr)
        .await
        .expect("bind ephemeral failed");
    let addr = listener
        .local_addr()
        .expect("failed to read local addr for listener");
    drop(listener);

    let handler = BrokerHandler::new(broker);
    let auth_validator =
        Arc::new(StaticApiKeyValidator::from_keys(["test-api-key"]));
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

struct TestClient {
    stream: TcpStream,
    buf: BytesMut,
}

impl TestClient {
    async fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr)
            .await
            .expect("failed to connect to server");
        Self {
            stream,
            buf: BytesMut::with_capacity(4096),
        }
    }

    async fn send_frame(&mut self, frame: &Frame) {
        let mut buf = BytesMut::new();
        encode_frame(frame, &mut buf).expect("encode_frame failed");
        self.stream
            .write_all(&buf)
            .await
            .expect("failed to write frame");
    }

    async fn recv_frame(&mut self) -> Option<Frame> {
        loop {
            if let Some(frame) = try_decode_frame(&mut self.buf)
                .expect("decode failed")
            {
                return Some(frame);
            }

            let n = self
                .stream
                .read_buf(&mut self.buf)
                .await
                .expect("failed to read from stream");
            if n == 0 {
                return None;
            }
        }
    }

    async fn handshake(&mut self) {
        // HELLO
        let hello = HelloPayload {
            protocol_version: PROTOCOL_VERSION,
        }
        .encode();
        let hello_frame = Frame {
            msg_type: FrameType::Hello,
            correlation_id: 1,
            payload: hello,
        };
        self.send_frame(&hello_frame).await;
        let resp = self.recv_frame().await.expect("no HELLO response");
        assert_eq!(resp.msg_type, FrameType::Ack);

        // AUTH
        let auth_payload = AuthPayload {
            api_key: "test-api-key".to_string(),
        }
        .encode()
        .expect("encode auth");
        let auth_frame = Frame {
            msg_type: FrameType::Auth,
            correlation_id: 2,
            payload: auth_payload,
        };
        self.send_frame(&auth_frame).await;
        let resp = self.recv_frame().await.expect("no AUTH response");
        assert_eq!(resp.msg_type, FrameType::Ack);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publish_receive_ack_qos1() {
    let broker = test_broker();
    let (addr, shutdown_tx) = start_test_server(broker.clone()).await;

    let mut client = TestClient::connect(addr).await;
    client.handshake().await;

    // SUBSCRIBE
    let sub_payload = SubscribePayload {
        topic: "integration".to_string(),
        qos: 1,
    }
    .encode()
    .expect("encode subscribe");
    let sub_frame = Frame {
        msg_type: FrameType::Subscribe,
        correlation_id: 10,
        payload: sub_payload,
    };
    client.send_frame(&sub_frame).await;
    let resp = client.recv_frame().await.expect("no SUBSCRIBE response");
    assert_eq!(resp.msg_type, FrameType::Ack);
    let ack = AckPayload::decode(&resp.payload).expect("decode ack");
    let sub_id = ack.subscription_id;
    assert_ne!(sub_id, 0);

    // PUBLISH
    let payload = PublishPayload {
        topic: "integration".to_string(),
        qos: 1,
        message: Bytes::from_static(b"hello"),
    }
    .encode()
    .expect("encode publish");
    let pub_frame = Frame {
        msg_type: FrameType::Publish,
        correlation_id: 20,
        payload,
    };
    client.send_frame(&pub_frame).await;

    // POLL
    let poll_payload = PollPayload { subscription_id: sub_id }
        .encode()
        .expect("encode poll");
    let poll_frame = Frame {
        msg_type: FrameType::Poll,
        correlation_id: 30,
        payload: poll_payload,
    };
    client.send_frame(&poll_frame).await;
    let resp = client.recv_frame().await.expect("no POLL response");
    assert_eq!(resp.msg_type, FrameType::Publish);

    let delivery = PublishPayload::decode(&resp.payload).expect("decode delivery");
    assert_eq!(delivery.topic, "integration");
    assert_eq!(delivery.qos, 1);
    assert_eq!(delivery.message, Bytes::from_static(b"hello"));

    // ACK
    let ack_payload = AckPayload { subscription_id: sub_id }
        .encode()
        .expect("encode ack");
    let ack_frame = Frame {
        msg_type: FrameType::Ack,
        correlation_id: resp.correlation_id,
        payload: ack_payload,
    };
    client.send_frame(&ack_frame).await;

    // Give server a moment to process the ACK.
    time::sleep(Duration::from_millis(20)).await;

    let _ = shutdown_tx.send(true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multiple_subscribers_receive_same_message() {
    let broker = test_broker();
    let (addr, shutdown_tx) = start_test_server(broker.clone()).await;

    let mut client = TestClient::connect(addr).await;
    client.handshake().await;

    // SUBSCRIBE 1
    let sub_payload1 = SubscribePayload {
        topic: "fanout".to_string(),
        qos: 1,
    }
    .encode()
    .expect("encode subscribe1");
    let sub_frame1 = Frame {
        msg_type: FrameType::Subscribe,
        correlation_id: 100,
        payload: sub_payload1,
    };
    client.send_frame(&sub_frame1).await;
    let resp1 = client.recv_frame().await.expect("no SUB1 response");
    let ack1 = AckPayload::decode(&resp1.payload).expect("decode ack1");
    let sub1 = ack1.subscription_id;

    // SUBSCRIBE 2
    let sub_payload2 = SubscribePayload {
        topic: "fanout".to_string(),
        qos: 1,
    }
    .encode()
    .expect("encode subscribe2");
    let sub_frame2 = Frame {
        msg_type: FrameType::Subscribe,
        correlation_id: 101,
        payload: sub_payload2,
    };
    client.send_frame(&sub_frame2).await;
    let resp2 = client.recv_frame().await.expect("no SUB2 response");
    let ack2 = AckPayload::decode(&resp2.payload).expect("decode ack2");
    let sub2 = ack2.subscription_id;

    // PUBLISH once.
    let payload = PublishPayload {
        topic: "fanout".to_string(),
        qos: 1,
        message: Bytes::from_static(b"fanout-msg"),
    }
    .encode()
    .expect("encode publish");
    let pub_frame = Frame {
        msg_type: FrameType::Publish,
        correlation_id: 200,
        payload,
    };
    client.send_frame(&pub_frame).await;

    // POLL for sub1.
    let poll1 = PollPayload { subscription_id: sub1 }
        .encode()
        .expect("encode poll1");
    let poll_frame1 = Frame {
        msg_type: FrameType::Poll,
        correlation_id: 201,
        payload: poll1,
    };
    client.send_frame(&poll_frame1).await;
    let resp_p1 = client.recv_frame().await.expect("no fanout resp1");
    assert_eq!(resp_p1.msg_type, FrameType::Publish);

    // POLL for sub2.
    let poll2 = PollPayload { subscription_id: sub2 }
        .encode()
        .expect("encode poll2");
    let poll_frame2 = Frame {
        msg_type: FrameType::Poll,
        correlation_id: 202,
        payload: poll2,
    };
    client.send_frame(&poll_frame2).await;
    let resp_p2 = client.recv_frame().await.expect("no fanout resp2");
    assert_eq!(resp_p2.msg_type, FrameType::Publish);

    let d1 = PublishPayload::decode(&resp_p1.payload).expect("decode d1");
    let d2 = PublishPayload::decode(&resp_p2.payload).expect("decode d2");
    assert_eq!(d1.message, Bytes::from_static(b"fanout-msg"));
    assert_eq!(d2.message, Bytes::from_static(b"fanout-msg"));

    let _ = shutdown_tx.send(true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_topic_and_ack_produce_nack() {
    let broker = test_broker();
    let (addr, shutdown_tx) = start_test_server(broker.clone()).await;

    let mut client = TestClient::connect(addr).await;
    client.handshake().await;

    // SUBSCRIBE with empty topic -> NACK 400 "empty topic".
    let sub_payload = SubscribePayload {
        topic: "".to_string(),
        qos: 1,
    }
    .encode()
    .expect("encode bad subscribe");
    let sub_frame = Frame {
        msg_type: FrameType::Subscribe,
        correlation_id: 300,
        payload: sub_payload,
    };
    client.send_frame(&sub_frame).await;
    let resp = client.recv_frame().await.expect("no response for bad sub");
    assert_eq!(resp.msg_type, FrameType::Nack);
    let nack = net::NackPayload::decode(&resp.payload).expect("decode nack");
    assert_eq!(nack.code, 400);

    // ACK with unknown subscription id -> NACK 404.
    let ack_payload = AckPayload {
        subscription_id: 999_999,
    }
    .encode()
    .expect("encode bad ack");
    let ack_frame = Frame {
        msg_type: FrameType::Ack,
        correlation_id: 12345,
        payload: ack_payload,
    };
    client.send_frame(&ack_frame).await;
    let resp = client.recv_frame().await.expect("no response for bad ack");
    assert_eq!(resp.msg_type, FrameType::Nack);
    let nack = net::NackPayload::decode(&resp.payload).expect("decode nack2");
    assert_eq!(nack.code, 404);

    let _ = shutdown_tx.send(true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_publish_payload_is_rejected() {
    let broker = test_broker();
    let (addr, shutdown_tx) = start_test_server(broker.clone()).await;

    let mut client = TestClient::connect(addr).await;
    client.handshake().await;

    // PUBLISH with invalid QoS value (e.g., 7) should generate NACK 400.
    let mut payload = BytesMut::new();
    payload.extend_from_slice(&[7u8]); // invalid QoS
    payload.extend_from_slice(&0u16.to_le_bytes()); // zero-length topic

    let frame = Frame {
        msg_type: FrameType::Publish,
        correlation_id: 400,
        payload: payload.freeze(),
    };

    client.send_frame(&frame).await;
    let resp = client.recv_frame().await.expect("no response for bad publish");
    assert_eq!(resp.msg_type, FrameType::Nack);
    let nack = net::NackPayload::decode(&resp.payload).expect("decode nack");
    assert_eq!(nack.code, 400);

    let _ = shutdown_tx.send(true);
}


