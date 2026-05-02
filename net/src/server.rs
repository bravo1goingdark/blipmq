use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use auth::ApiKeyValidator;
use bytes::Bytes;
use corelib::{
    Broker, ClientId, DeliveryEncoder, DeliveryTag, PushSender, PushSlot, QoSLevel, SubscriptionId,
    TopicName, WalError,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{debug, error, info};

use crate::connection::{BoxedReader, BoxedWriter, Connection};
use crate::error::Error;
use crate::frame::{
    AckPayload, Frame, FrameType, NackPayload, PollPayload, PublishPayload, SubscribePayload,
};

/// TLS configuration for the broker's TCP listener. When `Some` on
/// [`NetworkConfig`], every accepted connection is wrapped in a
/// `tokio_rustls::server::TlsStream`. PEM cert chain + PKCS#8 / RSA
/// key are loaded once at server start.
///
/// The struct is always defined so call sites can compile under either
/// feature flag; loading the actual cert/key requires the `tls` feature
/// to be enabled (otherwise `Server::start` returns an error).
#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub cert_chain: PathBuf,
    pub private_key: PathBuf,
}

#[derive(Debug, Clone)]
pub struct NetworkConfig {
    pub bind_addr: SocketAddr,
    /// When `Some`, all accepted connections are wrapped in a TLS handshake
    /// before any frames flow. Requires the `tls` feature.
    pub tls: Option<TlsConfig>,
}

impl NetworkConfig {
    /// Convenience: plain (non-TLS) config bound to `addr`.
    pub fn plain(bind_addr: SocketAddr) -> Self {
        Self {
            bind_addr,
            tls: None,
        }
    }
}

#[derive(Debug)]
pub enum FrameResponse {
    None,
    Frame(Frame),
}

#[async_trait]
pub trait MessageHandler: Send + Sync + 'static {
    async fn handle_frame(&self, conn_id: u64, frame: Frame) -> Result<FrameResponse, Error>;

    /// Handle a SUBSCRIBE frame in v2 push mode (legacy channel-based).
    /// The handler is given the connection's push Sender so it can register
    /// the subscription with the broker via `subscribe_with_conn`. Default
    /// impl falls back to the non-push `handle_frame`.
    async fn handle_subscribe_push(
        &self,
        conn_id: u64,
        frame: Frame,
        _push_tx: PushSender,
    ) -> Result<FrameResponse, Error> {
        self.handle_frame(conn_id, frame).await
    }

    /// Handle a SUBSCRIBE frame using the shared-buffer push path (v2 fast
    /// path). The handler registers the subscription via
    /// `subscribe_with_slot`, providing the slot + encoder pair created by
    /// the connection. Default impl falls back to the channel-based push.
    async fn handle_subscribe_slot(
        &self,
        conn_id: u64,
        frame: Frame,
        _slot: Arc<PushSlot>,
        _encoder: DeliveryEncoder,
    ) -> Result<FrameResponse, Error> {
        self.handle_frame(conn_id, frame).await
    }

    /// Sync fast path for PUBLISH frames. Avoids the per-frame
    /// `Box<dyn Future>` allocation and indirect call that `async_trait`
    /// imposes. Returns:
    ///   - `Ok(None)` on successful publish (no inband response).
    ///   - `Ok(Some(nack))` on parse / validation error.
    ///   - `Err(_)` on transport-fatal error.
    /// Implementations that need to await (e.g. QoS1 + WAL) may return
    /// `Ok(Some(_))` or fall back to the async path; the default impl
    /// here panics, since it must be overridden by any handler that
    /// participates in the v2 push hot path.
    fn handle_publish_fast(
        &self,
        _conn_id: u64,
        _frame: Frame,
        _topic_cache: &mut PublishTopicCache,
    ) -> Result<Option<Frame>, Error> {
        // The default impl can be safely "unreachable" because the
        // Connection only routes PUBLISH frames through this method when
        // the handler implements it; mocks / tests that do not opt in
        // never see PUBLISH on the fast path.
        Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "handle_publish_fast not implemented",
        )))
    }

    /// Notify the handler that a connection has dropped, so it can clean up
    /// any per-connection state (subscriptions, etc). Default: no-op.
    fn handle_connection_close(&self, _conn_id: u64) {}
}

/// Per-connection cache for the most recently resolved publish topic.
/// Same publisher publishes to the same topic millions of times in a
/// row; this single-slot cache turns each repeat lookup into a single
/// `Bytes` equality check + `Arc<str>` clone (refcount bump).
#[derive(Default)]
pub struct PublishTopicCache {
    last_bytes: Option<Bytes>,
    last_topic: Option<TopicName>,
}

impl PublishTopicCache {
    /// Fetch a `TopicName` for `topic_bytes`. If it matches the cached
    /// entry, returns a refcount-bumped clone; otherwise allocates a
    /// new `Arc<str>` and updates the cache.
    #[inline(always)]
    pub fn get(&mut self, topic_bytes: &Bytes) -> TopicName {
        if let (Some(prev_bytes), Some(prev_topic)) = (&self.last_bytes, &self.last_topic) {
            if prev_bytes == topic_bytes {
                return prev_topic.clone();
            }
        }
        // SAFETY: `PublishPayload::decode` validated UTF-8 already; this is
        // the same invariant `topic_str()` relies on.
        let s = unsafe { std::str::from_utf8_unchecked(topic_bytes) };
        let new_topic = TopicName::from_str(s);
        self.last_bytes = Some(topic_bytes.clone());
        self.last_topic = Some(new_topic.clone());
        new_topic
    }
}

pub struct Server<H>
where
    H: MessageHandler + Clone,
{
    config: NetworkConfig,
    handler: H,
    shutdown: watch::Receiver<bool>,
    auth_validator: Arc<dyn ApiKeyValidator>,
}

impl<H> Server<H>
where
    H: MessageHandler + Clone,
{
    pub fn new(
        config: NetworkConfig,
        handler: H,
        auth_validator: Arc<dyn ApiKeyValidator>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self {
            config,
            handler,
            shutdown,
            auth_validator,
        }
    }

    /// Run the accept loop until a shutdown signal is received.
    pub async fn start(&self) -> Result<(), Error> {
        let span = tracing::info_span!("net_server", bind_addr = %self.config.bind_addr);
        let _guard = span.enter();
        let listener = TcpListener::bind(self.config.bind_addr).await?;
        let local_addr = listener.local_addr()?;

        // Pre-build a TLS acceptor if configured. This validates the cert
        // and key paths once at startup rather than per-connection. When
        // the `tls` feature is off but the user supplied a `TlsConfig`,
        // we refuse to start so misconfiguration is surfaced loudly.
        let tls_acceptor = build_tls_acceptor(self.config.tls.as_ref())?;

        if tls_acceptor.is_some() {
            info!("net listening on {} (TLS)", local_addr);
        } else {
            info!("net listening on {}", local_addr);
        }

        let mut next_conn_id: u64 = 1;
        let mut shutdown_rx = self.shutdown.clone();

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, addr)) => {
                            let conn_id = next_conn_id;
                            next_conn_id = next_conn_id.wrapping_add(1);

                            let handler = self.handler.clone();
                            let conn_shutdown = shutdown_rx.clone();
                            let auth = self.auth_validator.clone();
                            let tls = tls_acceptor.clone();

                            // Disable Nagle: writer task issues already-batched frames; we want
                            // each batched write on the wire immediately, not coalesced again.
                            if let Err(err) = stream.set_nodelay(true) {
                                debug!("set_nodelay failed on conn {}: {}", conn_id, err);
                            }

                            debug!("accepted connection {} from {}", conn_id, addr);

                            // Hand off the TLS handshake (if any) and the
                            // connection's run-loop into a per-conn task
                            // so a slow handshake doesn't block accept.
                            tokio::spawn(async move {
                                let split = match accept_and_split(stream, tls.as_ref()).await {
                                    Ok(pair) => pair,
                                    Err(err) => {
                                        debug!(
                                            "conn {} accept/handshake failed: {}",
                                            conn_id, err
                                        );
                                        return;
                                    }
                                };
                                let (read_half, write_half) = split;
                                let connection = Connection::new(
                                    conn_id,
                                    read_half,
                                    write_half,
                                    handler,
                                    auth,
                                    conn_shutdown,
                                );
                                connection.run().await;
                            });
                        }
                        Err(err) => {
                            error!("accept error: {}", err);
                        }
                    }
                }
                result = shutdown_rx.changed() => {
                    match result {
                        Ok(_) => {
                            info!("shutdown signal received; stopping accept loop");
                            break;
                        }
                        Err(_) => {
                            info!("shutdown sender dropped; stopping accept loop");
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

#[derive(Clone)]
pub struct BrokerHandler {
    broker: Arc<Broker>,
}

impl BrokerHandler {
    pub fn new(broker: Arc<Broker>) -> Self {
        Self { broker }
    }

    fn qos_from_u8(&self, v: u8) -> Option<QoSLevel> {
        match v {
            0 => Some(QoSLevel::AtMostOnce),
            1 => Some(QoSLevel::AtLeastOnce),
            _ => None,
        }
    }

    /// Validate a topic name string. Returns `Some(reason)` if invalid;
    /// `None` if acceptable. Rules:
    /// - Non-empty.
    /// - Length must fit in the wire format's u16 (max 65535 bytes).
    /// - No NUL bytes (would confuse log lines, debug tooling, and some
    ///   clients).
    fn validate_topic(topic: &str) -> Option<&'static str> {
        if topic.is_empty() {
            return Some("empty topic");
        }
        if topic.len() > u16::MAX as usize {
            return Some("topic too long");
        }
        if topic.as_bytes().contains(&0) {
            return Some("topic contains NUL byte");
        }
        None
    }

    fn make_nack(&self, correlation_id: u64, code: u16, message: &str) -> Result<Frame, Error> {
        let payload = NackPayload {
            code,
            message: message.to_string(),
        }
        .encode()?;
        Ok(Frame {
            msg_type: FrameType::Nack,
            correlation_id,
            payload,
        })
    }
}

#[async_trait]
impl MessageHandler for BrokerHandler {
    async fn handle_frame(&self, conn_id: u64, frame: Frame) -> Result<FrameResponse, Error> {
        match frame.msg_type {
            FrameType::Publish => self.handle_publish(conn_id, frame).await,
            FrameType::Subscribe => self.handle_subscribe(conn_id, frame).await,
            FrameType::Ack => self.handle_ack(conn_id, frame).await,
            FrameType::Ping => {
                let pong = Frame {
                    msg_type: FrameType::Pong,
                    correlation_id: frame.correlation_id,
                    payload: Bytes::new(),
                };
                Ok(FrameResponse::Frame(pong))
            }
            FrameType::Poll => self.handle_poll(conn_id, frame).await,
            FrameType::Pong
            | FrameType::Nack
            | FrameType::Hello
            | FrameType::Auth
            | FrameType::Deliver => {
                // DELIVER is server-initiated; receiving one from the client is
                // protocol misuse but harmless to ignore here.
                Ok(FrameResponse::None)
            }
        }
    }

    async fn handle_subscribe_push(
        &self,
        conn_id: u64,
        frame: Frame,
        push_tx: PushSender,
    ) -> Result<FrameResponse, Error> {
        let decoded = SubscribePayload::decode(&frame.payload);
        let payload = match decoded {
            Ok(p) => p,
            Err(_) => {
                let nack =
                    self.make_nack(frame.correlation_id, 400, "invalid SUBSCRIBE payload")?;
                return Ok(FrameResponse::Frame(nack));
            }
        };

        if let Some(reason) = Self::validate_topic(&payload.topic) {
            let nack = self.make_nack(frame.correlation_id, 400, reason)?;
            return Ok(FrameResponse::Frame(nack));
        }

        let qos = match self.qos_from_u8(payload.qos) {
            Some(q) => q,
            None => {
                let nack = self.make_nack(frame.correlation_id, 400, "invalid QoS value")?;
                return Ok(FrameResponse::Frame(nack));
            }
        };

        let client_id = ClientId::new(format!("conn-{conn_id}"));
        let topic = TopicName::new(payload.topic);

        let sub_id = self
            .broker
            .subscribe_with_conn(client_id, topic, qos, conn_id, push_tx);

        let ack_payload = AckPayload {
            subscription_id: sub_id.value(),
        }
        .encode()?;

        Ok(FrameResponse::Frame(Frame {
            msg_type: FrameType::Ack,
            correlation_id: frame.correlation_id,
            payload: ack_payload,
        }))
    }

    async fn handle_subscribe_slot(
        &self,
        conn_id: u64,
        frame: Frame,
        slot: Arc<PushSlot>,
        encoder: DeliveryEncoder,
    ) -> Result<FrameResponse, Error> {
        let payload = match SubscribePayload::decode(&frame.payload) {
            Ok(p) => p,
            Err(_) => {
                return Ok(FrameResponse::Frame(self.make_nack(
                    frame.correlation_id,
                    400,
                    "invalid SUBSCRIBE payload",
                )?));
            }
        };

        if let Some(reason) = Self::validate_topic(&payload.topic) {
            return Ok(FrameResponse::Frame(self.make_nack(
                frame.correlation_id,
                400,
                reason,
            )?));
        }

        let qos = match self.qos_from_u8(payload.qos) {
            Some(q) => q,
            None => {
                return Ok(FrameResponse::Frame(self.make_nack(
                    frame.correlation_id,
                    400,
                    "invalid QoS value",
                )?));
            }
        };

        let client_id = ClientId::new(format!("conn-{conn_id}"));
        let topic = TopicName::new(payload.topic);

        let sub_id = self
            .broker
            .subscribe_with_slot(client_id, topic, qos, conn_id, slot, encoder);

        let ack_payload = AckPayload {
            subscription_id: sub_id.value(),
        }
        .encode()?;

        Ok(FrameResponse::Frame(Frame {
            msg_type: FrameType::Ack,
            correlation_id: frame.correlation_id,
            payload: ack_payload,
        }))
    }

    fn handle_connection_close(&self, conn_id: u64) {
        self.broker.unsubscribe_connection(conn_id);
    }

    fn handle_publish_fast(
        &self,
        _conn_id: u64,
        frame: Frame,
        topic_cache: &mut PublishTopicCache,
    ) -> Result<Option<Frame>, Error> {
        // Sync fast path: decode + dispatch with no async_trait box and no
        // .await. QoS1 + WAL still needs an awaitable fsync, so for that
        // case we fall through and force a NACK so the caller can retry on
        // the async path. (In practice clients targeting v2 either use
        // QoS0 here for max throughput or run a separate publisher
        // strategy for durable QoS1.)
        let payload = match PublishPayload::decode(&frame.payload) {
            Ok(p) => p,
            Err(_) => {
                return Ok(Some(self.make_nack(
                    frame.correlation_id,
                    400,
                    "invalid PUBLISH payload",
                )?));
            }
        };

        if let Some(reason) = Self::validate_topic(payload.topic_str()) {
            return Ok(Some(self.make_nack(frame.correlation_id, 400, reason)?));
        }

        let qos = match self.qos_from_u8(payload.qos) {
            Some(q) => q,
            None => {
                return Ok(Some(self.make_nack(
                    frame.correlation_id,
                    400,
                    "invalid QoS value",
                )?));
            }
        };

        // QoS1 + WAL must take the durable async path; punt back via a
        // sentinel error so the reader retries on handle_frame. For QoS0
        // (and QoS1 without WAL), publish synchronously.
        if qos == QoSLevel::AtLeastOnce && self.broker.has_wal() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "qos1_durable_requires_async",
            )));
        }

        let topic = topic_cache.get(&payload.topic);
        let ttl_override = payload
            .ttl_ms
            .map(|ms| std::time::Duration::from_millis(ms as u64));
        if ttl_override.is_some() {
            self.broker
                .publish_with_ttl(&topic, payload.message, qos, ttl_override);
        } else {
            self.broker.publish(&topic, payload.message, qos);
        }
        Ok(None)
    }
}

impl BrokerHandler {
    async fn handle_publish(&self, _conn_id: u64, frame: Frame) -> Result<FrameResponse, Error> {
        let decoded = PublishPayload::decode(&frame.payload);
        let payload = match decoded {
            Ok(p) => p,
            Err(_) => {
                let nack = self.make_nack(frame.correlation_id, 400, "invalid PUBLISH payload")?;
                return Ok(FrameResponse::Frame(nack));
            }
        };

        if let Some(reason) = Self::validate_topic(payload.topic_str()) {
            let nack = self.make_nack(frame.correlation_id, 400, reason)?;
            return Ok(FrameResponse::Frame(nack));
        }

        let qos = match self.qos_from_u8(payload.qos) {
            Some(q) => q,
            None => {
                let nack = self.make_nack(frame.correlation_id, 400, "invalid QoS value")?;
                return Ok(FrameResponse::Frame(nack));
            }
        };

        let topic = TopicName::from_str(payload.topic_str());
        let ttl_override = payload
            .ttl_ms
            .map(|ms| std::time::Duration::from_millis(ms as u64));
        if qos == QoSLevel::AtLeastOnce && self.broker.has_wal() {
            // Use the WAL-backed path when configured for QoS1 messages.
            // (Per-message TTL is honored only on the non-durable path
            // for now; durable publishes use BrokerConfig::message_ttl.
            // A follow-up can encode ttl_ms into the WAL message record
            // so durable replays preserve it.)
            let _ = ttl_override;
            if let Err(e) = self
                .broker
                .publish_durable(&topic, payload.message, qos)
                .await
            {
                // Distinguish transient overload (WAL channel full / writer
                // stopped) from real broker errors so clients can retry
                // with backoff on the former without confusing it with
                // a bug.
                let (code, label) = match &e {
                    WalError::Backpressure(_) => (503, "wal_busy"),
                    WalError::WriterStopped => (503, "wal_writer_stopped"),
                    _ => (500, "durable_publish_failed"),
                };
                let nack = self.make_nack(
                    frame.correlation_id,
                    code,
                    &format!("{label}: {e}"),
                )?;
                return Ok(FrameResponse::Frame(nack));
            }
        } else if ttl_override.is_some() {
            self.broker
                .publish_with_ttl(&topic, payload.message, qos, ttl_override);
        } else {
            self.broker.publish(&topic, payload.message, qos);
        }

        Ok(FrameResponse::None)
    }

    async fn handle_subscribe(&self, conn_id: u64, frame: Frame) -> Result<FrameResponse, Error> {
        let decoded = SubscribePayload::decode(&frame.payload);
        let payload = match decoded {
            Ok(p) => p,
            Err(_) => {
                let nack =
                    self.make_nack(frame.correlation_id, 400, "invalid SUBSCRIBE payload")?;
                return Ok(FrameResponse::Frame(nack));
            }
        };

        if let Some(reason) = Self::validate_topic(&payload.topic) {
            let nack = self.make_nack(frame.correlation_id, 400, reason)?;
            return Ok(FrameResponse::Frame(nack));
        }

        let qos = match self.qos_from_u8(payload.qos) {
            Some(q) => q,
            None => {
                let nack = self.make_nack(frame.correlation_id, 400, "invalid QoS value")?;
                return Ok(FrameResponse::Frame(nack));
            }
        };

        let client_id = ClientId::new(format!("conn-{conn_id}"));
        let topic = TopicName::new(payload.topic);

        let sub_id = self.broker.subscribe(client_id, topic, qos);

        let ack_payload = AckPayload {
            subscription_id: sub_id.value(),
        }
        .encode()?;

        let ack_frame = Frame {
            msg_type: FrameType::Ack,
            correlation_id: frame.correlation_id,
            payload: ack_payload,
        };

        Ok(FrameResponse::Frame(ack_frame))
    }

    async fn handle_ack(&self, _conn_id: u64, frame: Frame) -> Result<FrameResponse, Error> {
        let decoded = AckPayload::decode(&frame.payload);
        let payload = match decoded {
            Ok(p) => p,
            Err(_) => {
                let nack = self.make_nack(frame.correlation_id, 400, "invalid ACK payload")?;
                return Ok(FrameResponse::Frame(nack));
            }
        };

        if payload.subscription_id == 0 {
            let nack = self.make_nack(
                frame.correlation_id,
                400,
                "subscription_id must be non-zero",
            )?;
            return Ok(FrameResponse::Frame(nack));
        }

        let sub_id = SubscriptionId::from_raw(payload.subscription_id);
        let tag = DeliveryTag::from_raw(frame.correlation_id);

        if self.broker.ack(sub_id, tag) {
            Ok(FrameResponse::None)
        } else {
            let nack = self.make_nack(
                frame.correlation_id,
                404,
                "unknown subscription or delivery tag",
            )?;
            Ok(FrameResponse::Frame(nack))
        }
    }

    async fn handle_poll(&self, _conn_id: u64, frame: Frame) -> Result<FrameResponse, Error> {
        let decoded = PollPayload::decode(&frame.payload);
        let payload = match decoded {
            Ok(p) => p,
            Err(_) => {
                let nack = self.make_nack(frame.correlation_id, 400, "invalid POLL payload")?;
                return Ok(FrameResponse::Frame(nack));
            }
        };

        if payload.subscription_id == 0 {
            let nack = self.make_nack(
                frame.correlation_id,
                400,
                "subscription_id must be non-zero",
            )?;
            return Ok(FrameResponse::Frame(nack));
        }

        let sub_id = SubscriptionId::from_raw(payload.subscription_id);
        let polled = match self.broker.poll(sub_id) {
            Some(m) => m,
            None => return Ok(FrameResponse::None),
        };

        let qos_byte = match polled.qos {
            QoSLevel::AtMostOnce => 0,
            QoSLevel::AtLeastOnce => 1,
        };

        let payload_bytes = PublishPayload {
            topic: Bytes::copy_from_slice(polled.topic.as_str().as_bytes()),
            qos: qos_byte,
            message: polled.payload,
            ttl_ms: None,
        }
        .encode()?;

        let correlation_id = match polled.delivery_tag {
            Some(tag) => tag.value(),
            None => frame.correlation_id,
        };

        let publish_frame = Frame {
            msg_type: FrameType::Publish,
            correlation_id,
            payload: payload_bytes,
        };

        Ok(FrameResponse::Frame(publish_frame))
    }
}

// ---- TLS helpers ----------------------------------------------------------

/// Build a TLS acceptor from the configured cert+key. Returns `Ok(None)`
/// when `cfg` is `None` (plain TCP); errors loudly when `cfg` is `Some`
/// but the `tls` feature is off, so misconfiguration doesn't silently
/// degrade to plain TCP.
#[cfg(feature = "tls")]
fn build_tls_acceptor(
    cfg: Option<&TlsConfig>,
) -> Result<Option<tokio_rustls::TlsAcceptor>, Error> {
    use std::io::BufReader;
    use tokio_rustls::rustls::{pki_types::PrivateKeyDer, ServerConfig};
    use tokio_rustls::TlsAcceptor;

    let Some(cfg) = cfg else { return Ok(None) };

    let cert_file = std::fs::File::open(&cfg.cert_chain).map_err(Error::Io)?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Error::Io)?;
    if certs.is_empty() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "TLS cert chain file contained no PEM certificates",
        )));
    }

    let key_file = std::fs::File::open(&cfg.private_key).map_err(Error::Io)?;
    let mut key_reader = BufReader::new(key_file);
    // Try PKCS#8 first (modern), fall back to RSA-style PKCS#1.
    let key: PrivateKeyDer<'static> = if let Some(k) =
        rustls_pemfile::pkcs8_private_keys(&mut key_reader)
            .next()
            .transpose()
            .map_err(Error::Io)?
    {
        PrivateKeyDer::Pkcs8(k)
    } else {
        let key_file = std::fs::File::open(&cfg.private_key).map_err(Error::Io)?;
        let mut key_reader = BufReader::new(key_file);
        let k = rustls_pemfile::rsa_private_keys(&mut key_reader)
            .next()
            .ok_or_else(|| {
                Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "no PKCS#8 or RSA private key found in TLS key file",
                ))
            })?
            .map_err(Error::Io)?;
        PrivateKeyDer::Pkcs1(k)
    };

    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;

    Ok(Some(TlsAcceptor::from(Arc::new(server_config))))
}

#[cfg(not(feature = "tls"))]
fn build_tls_acceptor(cfg: Option<&TlsConfig>) -> Result<Option<()>, Error> {
    if cfg.is_some() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "TLS configured but the `tls` feature is not enabled at compile time",
        )));
    }
    Ok(None)
}

/// Accept the connection and (if a TLS acceptor is configured) drive the
/// handshake before splitting into reader/writer halves. Returns the
/// type-erased halves the connection layer uses.
#[cfg(feature = "tls")]
async fn accept_and_split(
    stream: TcpStream,
    tls: Option<&tokio_rustls::TlsAcceptor>,
) -> Result<(BoxedReader, BoxedWriter), Error> {
    use std::pin::Pin;
    use tokio::io;
    if let Some(acceptor) = tls {
        let tls_stream = acceptor.accept(stream).await.map_err(Error::Io)?;
        let (r, w) = io::split(tls_stream);
        Ok((Pin::new(Box::new(r)), Pin::new(Box::new(w))))
    } else {
        let (r, w) = stream.into_split();
        Ok((Pin::new(Box::new(r)), Pin::new(Box::new(w))))
    }
}

#[cfg(not(feature = "tls"))]
async fn accept_and_split(
    stream: TcpStream,
    _tls: Option<&()>,
) -> Result<(BoxedReader, BoxedWriter), Error> {
    use std::pin::Pin;
    let (r, w) = stream.into_split();
    Ok((Pin::new(Box::new(r)), Pin::new(Box::new(w))))
}

