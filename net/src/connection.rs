use std::pin::Pin;
use std::sync::Arc;

use bytes::BytesMut;
use corelib::{DeliveryEncoder, PushSlot, QoSLevel};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tracing::{debug, error, trace};

/// Type-erased read half so the connection layer doesn't care whether
/// the underlying stream is a plain `TcpStream` or a `TlsStream` over one.
pub type BoxedReader = Pin<Box<dyn AsyncRead + Send + Unpin>>;
/// Type-erased write half, see [`BoxedReader`].
pub type BoxedWriter = Pin<Box<dyn AsyncWrite + Send + Unpin>>;

use auth::{ApiKey, ApiKeyValidator};

use crate::error::Error;
use crate::frame::{
    self, encode_deliver_frame, AckPayload, AuthPayload, Frame, FrameType, HelloPayload,
    NackPayload,
};
use crate::server::{FrameResponse, MessageHandler, PublishTopicCache};

const INITIAL_BUFFER_SIZE: usize = 16 * 1024;
/// Soft byte ceiling on a single `write_all` batch. The writer drains the
/// shared push slot into a local buffer; once the local buffer reaches this
/// size, we issue the syscall.
const MAX_BATCH_BYTES: usize = 256 * 1024;
/// Bound on the in-band response channel from reader to writer. Holds
/// ACK/NACK/PONG frames synthesized by the handler. 64 is plenty since
/// reader awaits the handler one frame at a time.
const INBAND_CHANNEL_CAPACITY: usize = 64;

pub struct Connection<H>
where
    H: MessageHandler + Clone,
{
    id: u64,
    read_half: BoxedReader,
    write_half: BoxedWriter,
    handler: H,
    shutdown: watch::Receiver<bool>,
    auth_validator: Arc<dyn ApiKeyValidator>,
}

impl<H> Connection<H>
where
    H: MessageHandler + Clone,
{
    /// Construct from already-split, type-erased halves. The caller
    /// (typically `Server::start`) is responsible for accepting the
    /// underlying stream — plain TCP or post-TLS-handshake — and
    /// splitting it via `tokio::io::split`.
    pub fn new(
        id: u64,
        read_half: BoxedReader,
        write_half: BoxedWriter,
        handler: H,
        auth_validator: Arc<dyn ApiKeyValidator>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self {
            id,
            read_half,
            write_half,
            handler,
            shutdown,
            auth_validator,
        }
    }

    pub async fn run(self) {
        let span = tracing::info_span!("connection", conn_id = self.id);
        let _guard = span.enter();

        let conn_id = self.id;
        let handler = self.handler.clone();

        let read_half = self.read_half;
        let write_half = self.write_half;

        // Per-conn shared push slot: broker encodes DELIVER frames directly
        // into `slot.buf` under the parking_lot mutex and pings
        // `slot.notify`. Writer task wakes, swaps `slot.buf` for an empty
        // local buffer, and `write_all`s the swapped buffer.
        //
        // This replaces the previous flume::Sender<DeliveryHandle> hop with
        // a single mutex acquire + `notify_one` per delivery. The writer's
        // per-frame encode step is also gone — encoding happens inline on
        // the broker side via the `DeliveryEncoder` registered at
        // subscribe time.
        let push_slot = Arc::new(PushSlot::new(INITIAL_BUFFER_SIZE));

        // In-band channel: reader -> writer for handler-synthesized frames
        // (ACK/NACK/PONG/Subscribe-ACK/etc). Stays on tokio::sync::mpsc
        // because it's not on the hot path.
        let (inband_tx, inband_rx) = mpsc::channel::<Frame>(INBAND_CHANNEL_CAPACITY);

        let writer_shutdown = self.shutdown.clone();
        let writer_slot = push_slot.clone();
        let writer_jh = tokio::spawn(writer_task(
            conn_id,
            write_half,
            writer_slot,
            inband_rx,
            writer_shutdown,
        ));

        let reader_shutdown = self.shutdown.clone();
        let reader_handler = handler.clone();
        let reader_auth = self.auth_validator.clone();
        let reader_slot = push_slot.clone();
        let reader_jh = tokio::spawn(async move {
            let mut reader = ReaderState::new(
                conn_id,
                read_half,
                reader_handler,
                reader_auth,
                reader_shutdown,
                reader_slot,
                inband_tx,
            );
            if let Err(err) = reader.run().await {
                error!("connection {} reader error: {}", conn_id, err);
            }
        });

        // When either task ends, we want both to wind down. The reader
        // closes inband_tx and push_tx as it drops them; the writer exits
        // cleanly once both receivers are closed.
        let _ = reader_jh.await;
        let _ = writer_jh.await;

        handler.handle_connection_close(conn_id);
        trace!("connection {} closed", conn_id);
    }
}

// ---- writer task -----------------------------------------------------------

async fn writer_task(
    conn_id: u64,
    mut write_half: BoxedWriter,
    slot: Arc<PushSlot>,
    mut inband_rx: mpsc::Receiver<Frame>,
    mut shutdown: watch::Receiver<bool>,
) {
    // Local "active" buffer: we swap it for the shared `slot.buf` so the
    // broker never blocks on us holding the lock during write_all.
    let mut local = BytesMut::with_capacity(INITIAL_BUFFER_SIZE);

    loop {
        // 1. Take whatever the broker has accumulated in the shared slot.
        {
            let mut shared = slot.buf.lock();
            if !shared.is_empty() {
                std::mem::swap(&mut *shared, &mut local);
            }
        }

        // 2. Drain any in-band responses (ACK/NACK/PONG) that piled up.
        while local.len() < MAX_BATCH_BYTES {
            match inband_rx.try_recv() {
                Ok(f) => {
                    if frame::encode_frame(&f, &mut local).is_err() {
                        break;
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }

        // 3. Flush whatever we have in one syscall (TCP_NODELAY is on, so
        //    the kernel hands it straight to the wire).
        if !local.is_empty() {
            if let Err(err) = write_half.write_all(&local).await {
                debug!("conn {} write error: {}", conn_id, err);
                return;
            }
            local.clear();
            continue; // immediately re-check the slot in case more arrived
        }

        // 4. Nothing to flush -- wait for either a push notification or a
        //    new inband frame. `notified()` consumes any pending permit
        //    that fired between (1) and now, so we don't deadlock.
        let notified = slot.notify.notified();
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            _ = notified => {}
            f = inband_rx.recv() => {
                match f {
                    Some(f) => {
                        if frame::encode_frame(&f, &mut local).is_err() {
                            return;
                        }
                    }
                    None => {
                        // Reader dropped; finish anything pending in the
                        // shared slot, then exit. Lock-and-swap, then drop
                        // the guard before awaiting (parking_lot guards
                        // aren't Send).
                        {
                            let mut shared = slot.buf.lock();
                            if !shared.is_empty() {
                                std::mem::swap(&mut *shared, &mut local);
                            }
                        }
                        if !local.is_empty() {
                            let _ = write_half.write_all(&local).await;
                        }
                        break;
                    }
                }
            }
        }
    }

    // Best-effort graceful close.
    let _ = write_half.shutdown().await;
}

/// Build the encoder closure used by the broker on every push delivery.
/// Encodes a complete DELIVER frame (length-prefix + header + payload) into
/// the shared push buffer with no intermediate allocations.
fn make_deliver_encoder() -> DeliveryEncoder {
    Arc::new(|buf, qos, tag, topic, payload| {
        let qos_byte = match qos {
            QoSLevel::AtMostOnce => 0,
            QoSLevel::AtLeastOnce => 1,
        };
        // encode_deliver_frame returns Err only on absurdly large payloads
        // (>16 MiB topic/message); in that case we drop the frame rather
        // than poison the buffer mid-encode. The broker's QoS1 inflight
        // tracking still has the record so retries can recover.
        let _ = encode_deliver_frame(buf, qos_byte, tag, topic, payload);
    })
}

// ---- reader task -----------------------------------------------------------

struct ReaderState<H>
where
    H: MessageHandler + Clone,
{
    id: u64,
    read_half: BoxedReader,
    handler: H,
    auth_validator: Arc<dyn ApiKeyValidator>,
    shutdown: watch::Receiver<bool>,
    push_slot: Arc<PushSlot>,
    push_encoder: DeliveryEncoder,
    inband_tx: mpsc::Sender<Frame>,
    read_buf: BytesMut,
    hello_performed: bool,
    authenticated: bool,
    /// Number of failed AUTH attempts on this connection. Each failure
    /// triggers an exponential backoff before the NACK goes out, and the
    /// connection is closed once the count reaches `MAX_AUTH_FAILURES`.
    /// Resets to 0 on a successful AUTH.
    auth_failures: u32,
    /// Single-slot cache of the most-recently resolved publish topic.
    /// Common publisher pattern is "publish 1M to one topic" — this turns
    /// per-frame `Arc<str>` allocation into a refcount bump.
    topic_cache: PublishTopicCache,
}

/// Hard cap on consecutive failed AUTH attempts on one connection. After
/// this many failures the conn is closed; legitimate clients with a bad
/// key well below this limit hit only the exponential backoff.
const MAX_AUTH_FAILURES: u32 = 5;

impl<H> ReaderState<H>
where
    H: MessageHandler + Clone,
{
    fn new(
        id: u64,
        read_half: BoxedReader,
        handler: H,
        auth_validator: Arc<dyn ApiKeyValidator>,
        shutdown: watch::Receiver<bool>,
        push_slot: Arc<PushSlot>,
        inband_tx: mpsc::Sender<Frame>,
    ) -> Self {
        Self {
            id,
            read_half,
            handler,
            auth_validator,
            shutdown,
            push_slot,
            push_encoder: make_deliver_encoder(),
            inband_tx,
            read_buf: BytesMut::with_capacity(INITIAL_BUFFER_SIZE),
            hello_performed: false,
            authenticated: false,
            auth_failures: 0,
            topic_cache: PublishTopicCache::default(),
        }
    }

    async fn run(&mut self) -> Result<(), Error> {
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.changed() => return Ok(()),
                read_result = self.read_half.read_buf(&mut self.read_buf) => {
                    let n = read_result?;
                    if n == 0 {
                        return Ok(());
                    }
                    while let Some(frame) = frame::try_decode_frame(&mut self.read_buf)? {
                        self.process_frame(frame).await?;
                    }
                }
            }
        }
    }

    async fn process_frame(&mut self, frame: Frame) -> Result<(), Error> {
        match frame.msg_type {
            FrameType::Hello => self.handle_hello(frame).await,
            FrameType::Auth => self.handle_auth(frame).await,
            _ => {
                if !self.authenticated {
                    self.send_nack(frame.correlation_id, 401, "unauthenticated")
                        .await?;
                    return Ok(());
                }

                // PUBLISH hot path: try the sync fast path first. Avoids
                // the per-frame async_trait Box<dyn Future> alloc on the
                // dominant frame type. If the fast path returns
                // Unsupported (e.g. QoS1+WAL needs an awaitable fsync),
                // fall through to the async handle_frame path.
                if frame.msg_type == FrameType::Publish {
                    match self.handler.handle_publish_fast(
                        self.id,
                        frame.clone(),
                        &mut self.topic_cache,
                    ) {
                        Ok(None) => return Ok(()),
                        Ok(Some(nack)) => return self.send_inband(nack).await,
                        Err(Error::Io(e))
                            if e.kind() == std::io::ErrorKind::Unsupported =>
                        {
                            // Falls through to the async path below.
                        }
                        Err(e) => return Err(e),
                    }
                }

                let response = if frame.msg_type == FrameType::Subscribe {
                    self.handler
                        .handle_subscribe_slot(
                            self.id,
                            frame,
                            self.push_slot.clone(),
                            self.push_encoder.clone(),
                        )
                        .await
                } else {
                    self.handler.handle_frame(self.id, frame).await
                };

                match response {
                    Ok(FrameResponse::None) => Ok(()),
                    Ok(FrameResponse::Frame(resp)) => self.send_inband(resp).await,
                    Err(err) => Err(err),
                }
            }
        }
    }

    async fn handle_hello(&mut self, frame: Frame) -> Result<(), Error> {
        if self.hello_performed {
            return self
                .send_nack(frame.correlation_id, 400, "HELLO already performed")
                .await;
        }

        match HelloPayload::decode(&frame.payload) {
            Ok(payload) => {
                if payload.protocol_version != frame::PROTOCOL_VERSION {
                    return self
                        .send_nack(frame.correlation_id, 426, "unsupported protocol version")
                        .await;
                }
                self.hello_performed = true;
                self.send_ack(frame.correlation_id).await
            }
            Err(_) => {
                self.send_nack(frame.correlation_id, 400, "invalid HELLO payload")
                    .await
            }
        }
    }

    async fn handle_auth(&mut self, frame: Frame) -> Result<(), Error> {
        if !self.hello_performed {
            return self
                .send_nack(frame.correlation_id, 400, "HELLO not performed")
                .await;
        }
        if self.authenticated {
            return self
                .send_nack(frame.correlation_id, 400, "already authenticated")
                .await;
        }

        let payload = match AuthPayload::decode(&frame.payload) {
            Ok(p) => p,
            Err(_) => {
                return self
                    .send_nack(frame.correlation_id, 400, "invalid AUTH payload")
                    .await
            }
        };

        let api_key = ApiKey::new(payload.api_key);
        if self.auth_validator.validate(&api_key) {
            self.authenticated = true;
            self.auth_failures = 0;
            self.send_ack(frame.correlation_id).await
        } else {
            self.auth_failures = self.auth_failures.saturating_add(1);

            // Exponential backoff before the NACK goes out: 50ms, 100ms,
            // 200ms, 400ms, 800ms (capped at 1s). Discourages brute-force
            // key guessing without making a single typo painful for
            // legitimate clients.
            let base_ms = 50u64;
            let backoff_ms =
                base_ms.saturating_mul(1u64 << (self.auth_failures.min(5) - 1).min(4)).min(1000);
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;

            self.send_nack(frame.correlation_id, 401, "invalid API key")
                .await?;

            // Hard cap. Beyond this, close the connection so a brute-
            // forcer has to re-establish TCP (and re-pay TLS handshake
            // cost when TLS is on) for every additional batch of attempts.
            if self.auth_failures >= MAX_AUTH_FAILURES {
                debug!(
                    "conn {} closing after {} failed AUTH attempts",
                    self.id, self.auth_failures,
                );
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "too many failed AUTH attempts",
                )));
            }
            Ok(())
        }
    }

    #[inline]
    async fn send_inband(&mut self, frame: Frame) -> Result<(), Error> {
        if self.inband_tx.send(frame).await.is_err() {
            // Writer task gone; treat as connection-closed.
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "writer task closed",
            )));
        }
        Ok(())
    }

    async fn send_ack(&mut self, correlation_id: u64) -> Result<(), Error> {
        let payload = AckPayload { subscription_id: 0 }.encode()?;
        self.send_inband(Frame {
            msg_type: FrameType::Ack,
            correlation_id,
            payload,
        })
        .await
    }

    async fn send_nack(
        &mut self,
        correlation_id: u64,
        code: u16,
        message: &str,
    ) -> Result<(), Error> {
        let payload = NackPayload {
            code,
            message: message.to_string(),
        }
        .encode()?;
        self.send_inband(Frame {
            msg_type: FrameType::Nack,
            correlation_id,
            payload,
        })
        .await
    }
}

