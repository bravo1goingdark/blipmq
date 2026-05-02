//! End-to-end TLS handshake test for the broker's TCP listener.
//!
//! Generates a self-signed RSA cert + key at runtime via `rcgen`,
//! writes them to the test's temp dir, configures `Server` with TLS,
//! and runs a HELLO/AUTH/SUBSCRIBE/PUBLISH/DELIVER round-trip through
//! a `tokio_rustls` client that trusts the self-signed cert. Proves
//! the TLS wiring (config parsing, accept-time handshake, dyn-traited
//! connection halves) works end-to-end.
//!
//! Only built when the `tls` feature is enabled:
//!   `cargo test -p net --features tls --test tls_handshake`

#![cfg(feature = "tls")]

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
    encode_frame, try_decode_frame, AuthPayload, BrokerHandler, DeliverPayload, Frame, FrameType,
    HelloPayload, NetworkConfig, PublishPayload, Server, SubscribePayload, TlsConfig,
    PROTOCOL_VERSION,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

const API_KEY: &str = "tls-test-key";
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Generate a self-signed cert + key for `localhost`, write both to
/// `dir`, and return:
///   - the path to the cert (PEM),
///   - the path to the key (PEM, PKCS#8),
///   - the cert in DER form (so the test client can trust it without
///     a system CA).
fn generate_self_signed(
    dir: &std::path::Path,
) -> (std::path::PathBuf, std::path::PathBuf, Vec<u8>) {
    let cert_key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("rcgen self-signed");
    let cert_pem = cert_key.cert.pem();
    let key_pem = cert_key.key_pair.serialize_pem();
    let cert_der = cert_key.cert.der().to_vec();

    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, cert_pem).expect("write cert");
    std::fs::write(&key_path, key_pem).expect("write key");
    (cert_path, key_path, cert_der)
}

async fn read_until_frame(
    stream: &mut tokio_rustls::client::TlsStream<TcpStream>,
    buf: &mut BytesMut,
) -> Frame {
    loop {
        if let Some(f) = try_decode_frame(buf).unwrap() {
            return f;
        }
        let n = stream.read_buf(buf).await.unwrap();
        if n == 0 {
            panic!("connection closed before frame arrived");
        }
    }
}

#[tokio::test]
async fn tls_full_publish_subscribe_roundtrip() {
    // 1. Stage cert + key + DER for the test client.
    let tmp = tempfile::tempdir_in(std::env::temp_dir()).expect("tempdir");
    let (cert_path, key_path, cert_der) = generate_self_signed(tmp.path());

    // 2. Stand up the broker on TLS.
    let broker = Arc::new(Broker::new(BrokerConfig {
        default_qos: QoSLevel::AtMostOnce,
        message_ttl: Duration::from_secs(60),
        per_subscriber_queue_capacity: 1024,
        max_retries: 3,
        retry_base_delay: Duration::from_millis(50),
        ..Default::default()
    }));
    let handler = BrokerHandler::new(broker.clone());
    let auth = Arc::new(StaticApiKeyValidator::from_keys([API_KEY.to_string()]));
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local: SocketAddr = probe.local_addr().unwrap();
    drop(probe);

    let server = Server::new(
        NetworkConfig {
            bind_addr: local,
            tls: Some(TlsConfig {
                cert_chain: cert_path,
                private_key: key_path,
            }),
        },
        handler,
        auth,
        shutdown_rx,
    );
    tokio::spawn(async move {
        let _ = server.start().await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    // 3. Build a rustls client config that trusts our self-signed cert.
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(cert_der)).expect("add root");
    let client_cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_cfg));
    let server_name = ServerName::try_from("localhost").unwrap();

    // Helper: open one TLS conn, do HELLO/AUTH.
    let connect_and_handshake =
        |connector: TlsConnector, server_name: ServerName<'static>, addr: SocketAddr| async move {
            let raw = TcpStream::connect(addr).await.unwrap();
            raw.set_nodelay(true).unwrap();
            let mut tls = connector.connect(server_name, raw).await.unwrap();
            let mut buf = BytesMut::with_capacity(8 * 1024);

            // HELLO
            let mut out = BytesMut::new();
            encode_frame(
                &Frame {
                    msg_type: FrameType::Hello,
                    correlation_id: 1,
                    payload: HelloPayload {
                        protocol_version: PROTOCOL_VERSION,
                    }
                    .encode(),
                },
                &mut out,
            )
            .unwrap();
            tls.write_all(&out).await.unwrap();
            let resp = read_until_frame(&mut tls, &mut buf).await;
            assert_eq!(
                resp.msg_type,
                FrameType::Ack,
                "HELLO must be ACKed over TLS"
            );

            // AUTH
            out.clear();
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
                &mut out,
            )
            .unwrap();
            tls.write_all(&out).await.unwrap();
            let resp = read_until_frame(&mut tls, &mut buf).await;
            assert_eq!(resp.msg_type, FrameType::Ack);

            (tls, buf)
        };

    // 4. Subscriber connects + subscribes.
    let (mut sub_stream, mut sub_buf) =
        connect_and_handshake(connector.clone(), server_name.clone(), local).await;

    let mut out = BytesMut::new();
    encode_frame(
        &Frame {
            msg_type: FrameType::Subscribe,
            correlation_id: 10,
            payload: SubscribePayload {
                topic: "tls-roundtrip".to_string(),
                qos: 0,
                group: None,
                from_offset: None,
            }
            .encode()
            .unwrap(),
        },
        &mut out,
    )
    .unwrap();
    sub_stream.write_all(&out).await.unwrap();
    let resp = read_until_frame(&mut sub_stream, &mut sub_buf).await;
    assert_eq!(resp.msg_type, FrameType::Ack, "SUBSCRIBE must be ACKed");

    // 5. Publisher connects + publishes.
    let (mut pub_stream, _pub_buf) = connect_and_handshake(connector, server_name, local).await;
    out.clear();
    encode_frame(
        &Frame {
            msg_type: FrameType::Publish,
            correlation_id: 20,
            payload: PublishPayload {
                topic: Bytes::from_static(b"tls-roundtrip"),
                qos: 0,
                message: Bytes::from_static(b"over-tls"),
                ttl_ms: None,
            }
            .encode()
            .unwrap(),
        },
        &mut out,
    )
    .unwrap();
    pub_stream.write_all(&out).await.unwrap();

    // 6. Subscriber should receive the DELIVER frame through the TLS tunnel.
    let frame = timeout(
        TEST_TIMEOUT,
        read_until_frame(&mut sub_stream, &mut sub_buf),
    )
    .await
    .expect("DELIVER frame must arrive over TLS within timeout");
    assert_eq!(frame.msg_type, FrameType::Deliver);
    let payload = DeliverPayload::decode(&frame.payload).unwrap();
    assert_eq!(payload.topic, "tls-roundtrip");
    assert_eq!(payload.qos, 0);
    assert_eq!(payload.message.as_ref(), b"over-tls");
}
