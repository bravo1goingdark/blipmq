//! Server engine for BlipMQ broker.
//!
//! Uses length-prefixed FlatBuffers for all protocol messages.

use crate::config::CONFIG;
use crate::core::{
    command::{decode_command, Action, ClientCommand},
    message::{encode_frame_into, new_message_with_ttl, ServerFrame, SubAck},
    subscriber::{Subscriber, SubscriberId},
    topics::TopicRegistry,
};

use bytes::BytesMut;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::{
    io::{BufReader, BufWriter},
    net::{TcpListener, TcpStream},
    sync::Mutex,
    task,
};
use tracing::{error, info};

/// Reasonable caps to protect against malformed clients and reduce reallocs.
const INBUF_INIT: usize = 64 * 1024;
const MAX_FRAME_LEN: usize = 8 * 1024 * 1024; // 8 MiB cap per command

/// Starts the BlipMQ broker server, with settings from `blipmq.toml`.
pub async fn serve() -> anyhow::Result<()> {
    let bind_addr = &CONFIG.server.bind_addr;
    info!("Starting BlipMQ broker on {}", bind_addr);

    let listener = TcpListener::bind(bind_addr).await?;
    let registry = Arc::new(TopicRegistry::new());

    loop {
        let (socket, peer_addr) = listener.accept().await?;
        socket.set_nodelay(true)?;
        let registry = Arc::clone(&registry);
        info!("Client connected: {}", peer_addr);

        task::spawn(async move {
            if let Err(e) = handle_client(socket, registry).await {
                error!("Error handling {}: {:?}", peer_addr, e);
            }
        });
    }
}

async fn handle_client(stream: TcpStream, registry: Arc<TopicRegistry>) -> anyhow::Result<()> {
    let peer = stream.peer_addr()?;
    let (reader_half, writer_half) = stream.into_split();
    let mut reader = BufReader::new(reader_half);

    // Writer shared with Subscriber for server->client frames (SubAck, deliveries, etc.)
    let shared_writer = Arc::new(Mutex::new(BufWriter::new(writer_half)));

    let subscriber_id = SubscriberId::from(peer.to_string());
    let subscriber = Subscriber::new(subscriber_id.clone(), shared_writer.clone());

    // Reusable decode buffer for commands (length-prefixed).
    let mut inbuf = BytesMut::with_capacity(INBUF_INIT);

    // Track topics for cleanup.
    let mut subscriptions: Vec<String> = Vec::with_capacity(8);

    'io: loop {
        // Read more bytes from socket (coalesces many commands per syscall).
        let n = reader.read_buf(&mut inbuf).await?;
        if n == 0 {
            // EOF
            break 'io;
        }

        // Parse as many complete frames as available.
        loop {
            if inbuf.len() < 4 {
                // Not enough for length prefix yet.
                break;
            }

            // Peek length prefix (big-endian u32).
            let len = u32::from_be_bytes([inbuf[0], inbuf[1], inbuf[2], inbuf[3]]) as usize;

            // Guard against insane frames.
            if len > MAX_FRAME_LEN {
                error!(
                    "Client {} sent frame larger than MAX_FRAME_LEN ({} > {})",
                    peer, len, MAX_FRAME_LEN
                );
                // Drop connection to protect broker.
                break 'io;
            }

            if inbuf.len() < 4 + len {
                // Incomplete frame; read more.
                break;
            }

            // We have a full frame. Split it off in one shot:
            let frame = inbuf.split_to(4 + len);
            // Drop the 4-byte prefix and keep payload slice:
            let payload = &frame[4..];

            // Decode command (FlatBuffers).
            let cmd: ClientCommand = match decode_command(payload) {
                Ok(c) => c,
                Err(e) => {
                    error!("Failed to decode command from {}: {:?}", peer, e);
                    continue; // Skip this frame, keep the connection alive.
                }
            };

            match cmd.action {
                Action::Pub => {
                    if let Some(topic) = registry.get_topic(&cmd.topic) {
                        // Move payload into message (new_message_with_ttl takes ownership).
                        topic
                            .publish(Arc::new(new_message_with_ttl(cmd.payload, cmd.ttl_ms)))
                            .await;
                    }
                }

                Action::Sub => {
                    // 1) Send SubAck
                    let ack = SubAck {
                        topic: cmd.topic.clone(),
                        info: "subscribed".into(),
                    };
                    let frame = ServerFrame::SubAck(ack);

                    // Encode once into a reusable scratch buffer.
                    // Keep this small; a single ack is tiny.
                    let mut frame_buf = BytesMut::with_capacity(64);
                    encode_frame_into(&frame, &mut frame_buf);

                    {
                        let mut w = shared_writer.lock().await;
                        // write_all is buffered by BufWriter; cheap for small frames.
                        w.write_all(&frame_buf).await?;
                        // Flush to keep client UX snappy at subscribe time;
                        // this is rare compared to PUB traffic, so OK to flush here.
                        w.flush().await?;
                    }

                    // 2) Register subscription
                    let topic = registry.create_or_get_topic(&cmd.topic);
                    topic
                        .subscribe(subscriber.clone(), CONFIG.queues.subscriber_capacity)
                        .await;
                    subscriptions.push(cmd.topic);
                }

                Action::Unsub => {
                    if let Some(topic) = registry.get_topic(&cmd.topic) {
                        topic.unsubscribe(&subscriber_id).await;
                    }
                }

                Action::Quit => {
                    break 'io;
                }
            }
        }
    }

    // Cleanup: unsubscribe from all topics
    for topic in subscriptions {
        if let Some(t) = registry.get_topic(&topic) {
            t.unsubscribe(&subscriber_id).await;
        }
    }

    Ok(())
}
