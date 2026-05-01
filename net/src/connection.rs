use std::sync::Arc;

use bytes::BytesMut;
use corelib::{DeliveryHandle, PushReceiver, PushSender};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tracing::{debug, error, trace};

use auth::{ApiKey, ApiKeyValidator};

use crate::error::Error;
use crate::frame::{
    self, encode_deliver_frame, AckPayload, AuthPayload, Frame, FrameType, HelloPayload,
    NackPayload,
};
use crate::server::{FrameResponse, MessageHandler, PublishTopicCache};

const INITIAL_BUFFER_SIZE: usize = 16 * 1024;
/// Max frames coalesced into one writev/write_all on the wire.
const MAX_BATCH_FRAMES: usize = 128;
/// Max bytes coalesced into one writev/write_all on the wire.
const MAX_BATCH_BYTES: usize = 256 * 1024;
/// Bound on per-connection push channel; once full, broker increments
/// `push_dropped_total` and drops the message (slow-consumer policy in
/// Phase 6 will replace this with disconnect/configurable behavior).
const PUSH_CHANNEL_CAPACITY: usize = 65_536;
/// Bound on the in-band response channel from reader to writer. Holds
/// ACK/NACK/PONG frames synthesized by the handler. 64 is plenty since
/// reader awaits the handler one frame at a time.
const INBAND_CHANNEL_CAPACITY: usize = 64;

/// One frame to be written to the wire. Comes either from a server-initiated
/// push (DELIVER) or from a synchronous response synthesized by the reader
/// task (ACK / NACK / PONG / etc).
#[derive(Debug)]
enum WriterItem {
    Deliver(DeliveryHandle),
    Frame(Frame),
}

pub struct Connection<H>
where
    H: MessageHandler + Clone,
{
    id: u64,
    stream: TcpStream,
    handler: H,
    shutdown: watch::Receiver<bool>,
    auth_validator: Arc<dyn ApiKeyValidator>,
}

impl<H> Connection<H>
where
    H: MessageHandler + Clone,
{
    pub fn new(
        id: u64,
        stream: TcpStream,
        handler: H,
        auth_validator: Arc<dyn ApiKeyValidator>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self {
            id,
            stream,
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

        let (read_half, write_half) = self.stream.into_split();

        // Push channel: broker -> writer task. Sender is cloned into every
        // SUBSCRIBE for this connection; receiver lives in the writer task.
        // We use `flume` here (not `tokio::sync::mpsc`) because per-op
        // overhead measurably matters at multi-million msg/s; recv_async
        // integrates cleanly with tokio's runtime.
        let (push_tx, push_rx) = flume::bounded::<DeliveryHandle>(PUSH_CHANNEL_CAPACITY);

        // In-band channel: reader -> writer for handler-synthesized frames
        // (ACK/NACK/PONG/Subscribe-ACK/etc). Decouples reader from blocking
        // on the wire. Stays on tokio::sync::mpsc because (a) it's not on
        // the hot path and (b) we already use tokio::select! over both
        // channels together.
        let (inband_tx, inband_rx) = mpsc::channel::<Frame>(INBAND_CHANNEL_CAPACITY);

        let writer_shutdown = self.shutdown.clone();
        let writer_jh = tokio::spawn(writer_task(
            conn_id,
            write_half,
            push_rx,
            inband_rx,
            writer_shutdown,
        ));

        let reader_shutdown = self.shutdown.clone();
        let reader_handler = handler.clone();
        let reader_auth = self.auth_validator.clone();
        let reader_jh = tokio::spawn(async move {
            let mut reader = ReaderState::new(
                conn_id,
                read_half,
                reader_handler,
                reader_auth,
                reader_shutdown,
                push_tx,
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
    mut write_half: OwnedWriteHalf,
    push_rx: PushReceiver,
    mut inband_rx: mpsc::Receiver<Frame>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut buf = BytesMut::with_capacity(INITIAL_BUFFER_SIZE);

    loop {
        // Block until we have something to write, or shutdown. flume's
        // recv_async returns Err once the last Sender is dropped.
        let first = tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            d = push_rx.recv_async() => match d {
                Ok(d) => WriterItem::Deliver(d),
                Err(_) => {
                    // No more push senders; only inband can fire from now on.
                    match inband_rx.recv().await {
                        Some(f) => WriterItem::Frame(f),
                        None => break,
                    }
                }
            },
            f = inband_rx.recv() => match f {
                Some(f) => WriterItem::Frame(f),
                None => {
                    match push_rx.recv_async().await {
                        Ok(d) => WriterItem::Deliver(d),
                        Err(_) => break,
                    }
                }
            },
        };

        if let Err(err) = encode_into(&mut buf, first) {
            error!("conn {} encode error: {}", conn_id, err);
            return;
        }
        let mut frames_in_batch = 1usize;

        // Drain push channel as fast as possible — this is the hot path for
        // server-initiated DELIVERs. Only fall back to checking inband once
        // push is empty.
        while frames_in_batch < MAX_BATCH_FRAMES && buf.len() < MAX_BATCH_BYTES {
            match push_rx.try_recv() {
                Ok(d) => {
                    if encode_into(&mut buf, WriterItem::Deliver(d)).is_err() {
                        break;
                    }
                    frames_in_batch += 1;
                }
                Err(flume::TryRecvError::Empty) => break,
                Err(flume::TryRecvError::Disconnected) => break,
            }
        }
        // Drain any inband responses (ACK/NACK/PONG) that piled up.
        while frames_in_batch < MAX_BATCH_FRAMES && buf.len() < MAX_BATCH_BYTES {
            match inband_rx.try_recv() {
                Ok(f) => {
                    if encode_into(&mut buf, WriterItem::Frame(f)).is_err() {
                        break;
                    }
                    frames_in_batch += 1;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }

        // Single syscall for the whole batch (TCP_NODELAY ensures it goes
        // straight to the wire without additional Nagle coalescing).
        if let Err(err) = write_half.write_all(&buf).await {
            debug!("conn {} write error: {}", conn_id, err);
            return;
        }
        buf.clear();
    }

    // Best-effort graceful close. Ignore errors here; the peer may already
    // be gone.
    let _ = write_half.shutdown().await;
}

#[inline(always)]
fn encode_into(buf: &mut BytesMut, item: WriterItem) -> Result<(), Error> {
    match item {
        WriterItem::Deliver(d) => {
            let qos_byte = match d.qos {
                corelib::QoSLevel::AtMostOnce => 0,
                corelib::QoSLevel::AtLeastOnce => 1,
            };
            // Single-pass encode: no intermediate Bytes, no topic.to_string()
            // clone, no payload memcpy beyond the one into `buf`.
            encode_deliver_frame(buf, qos_byte, d.delivery_tag, d.topic.as_str(), &d.payload)?;
        }
        WriterItem::Frame(f) => {
            frame::encode_frame(&f, buf)?;
        }
    }
    Ok(())
}

// ---- reader task -----------------------------------------------------------

struct ReaderState<H>
where
    H: MessageHandler + Clone,
{
    id: u64,
    read_half: OwnedReadHalf,
    handler: H,
    auth_validator: Arc<dyn ApiKeyValidator>,
    shutdown: watch::Receiver<bool>,
    push_tx: PushSender,
    inband_tx: mpsc::Sender<Frame>,
    read_buf: BytesMut,
    hello_performed: bool,
    authenticated: bool,
    /// Single-slot cache of the most-recently resolved publish topic.
    /// Common publisher pattern is "publish 1M to one topic" — this turns
    /// per-frame `Arc<str>` allocation into a refcount bump.
    topic_cache: PublishTopicCache,
}

impl<H> ReaderState<H>
where
    H: MessageHandler + Clone,
{
    fn new(
        id: u64,
        read_half: OwnedReadHalf,
        handler: H,
        auth_validator: Arc<dyn ApiKeyValidator>,
        shutdown: watch::Receiver<bool>,
        push_tx: PushSender,
        inband_tx: mpsc::Sender<Frame>,
    ) -> Self {
        Self {
            id,
            read_half,
            handler,
            auth_validator,
            shutdown,
            push_tx,
            inband_tx,
            read_buf: BytesMut::with_capacity(INITIAL_BUFFER_SIZE),
            hello_performed: false,
            authenticated: false,
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
                        .handle_subscribe_push(self.id, frame, self.push_tx.clone())
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
            self.send_ack(frame.correlation_id).await
        } else {
            self.send_nack(frame.correlation_id, 401, "invalid API key")
                .await
        }
    }

    #[inline]
    async fn send_inband(&self, frame: Frame) -> Result<(), Error> {
        if self.inband_tx.send(frame).await.is_err() {
            // Writer task gone; treat as connection-closed.
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "writer task closed",
            )));
        }
        Ok(())
    }

    async fn send_ack(&self, correlation_id: u64) -> Result<(), Error> {
        let payload = AckPayload { subscription_id: 0 }.encode()?;
        self.send_inband(Frame {
            msg_type: FrameType::Ack,
            correlation_id,
            payload,
        })
        .await
    }

    async fn send_nack(
        &self,
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

