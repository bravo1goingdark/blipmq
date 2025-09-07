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

use bytes::{Bytes, BytesMut};
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    task,
};
use tracing::{error, info, warn};

/// Starts the BlipMQ broker server, with settings from `blipmq.toml`.
pub async fn serve() -> anyhow::Result<()> {
    let bind_addr = &CONFIG.server.bind_addr;
    info!("Starting BlipMQ broker on {}", bind_addr);

    let listener = TcpListener::bind(bind_addr).await?;
    let registry = Arc::new(TopicRegistry::new());

    // Spawn a very small HTTP metrics/health server on CONFIG.metrics.bind_addr
    let metrics_addr = CONFIG.metrics.bind_addr.clone();
    tokio::spawn(async move {
        if let Err(e) = serve_metrics(&metrics_addr).await {
            warn!("metrics server error: {:?}", e);
        }
    });

    // Mark broker as ready after listener is bound and metrics server spawned
    crate::metrics::set_ready(true);

    loop {
        let (socket, peer_addr) = listener.accept().await?;
        socket.set_nodelay(true)?;
        let registry = Arc::clone(&registry);
        info!("Client connected: {}", peer_addr);

        task::spawn(async move {
            if let Err(e) = handle_client(socket, registry).await {
                error!("Error handling client {}: {:?}", peer_addr, e);
            } else {
                info!("Client disconnected: {}", peer_addr);
            }
        });
    }
}

async fn serve_metrics(addr: &str) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!("metrics/health listening on {}", addr);
    loop {
        let (sock, _peer) = listener.accept().await?;
        // Very small, allocation-light request handler
        let mut reader = BufReader::new(sock);
        let mut buf = [0u8; 1024];
        let n: usize;
        match reader.read(&mut buf).await {
            Ok(0) => continue,
            Ok(m) => n = m,
            Err(e) => {
                warn!("metrics read error: {:?}", e);
                continue;
            }
        }
        let req = match std::str::from_utf8(&buf[..n]) {
            Ok(s) => s,
            Err(_) => "",
        };
        let mut path = "/metrics";
        let mut method = "GET";
        if let Some(line) = req.lines().next() {
            let mut parts = line.split_whitespace();
            method = parts.next().unwrap_or("GET");
            path = parts.next().unwrap_or("/metrics");
        }

        let (status, body, content_type): (&str, String, &str) = match (method, path) {
            ("GET", "/metrics") => (
                "200 OK",
                crate::metrics::snapshot(),
                "text/plain; charset=utf-8",
            ),
            ("GET", "/healthz") => ("200 OK", "ok".to_string(), "text/plain; charset=utf-8"),
            ("GET", "/readyz") => {
                if crate::metrics::is_ready() {
                    ("200 OK", "ready".to_string(), "text/plain; charset=utf-8")
                } else {
                    (
                        "503 Service Unavailable",
                        "not-ready".to_string(),
                        "text/plain; charset=utf-8",
                    )
                }
            }
            _ => (
                "404 Not Found",
                "not found".to_string(),
                "text/plain; charset=utf-8",
            ),
        };

        let resp = format!(
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status,
            content_type,
            body.len(),
            body
        );

        if let Err(e) = reader.get_mut().write_all(resp.as_bytes()).await {
            warn!("metrics write error: {:?}", e);
        }
    }
}

async fn handle_client(stream: TcpStream, registry: Arc<TopicRegistry>) -> anyhow::Result<()> {
    let peer = stream.peer_addr()?;
    let (reader_half, writer_half) = stream.into_split();
    let mut reader = BufReader::new(reader_half);

    // Per-connection writer task: all frames go through this channel
    let writer_tx = crate::core::subscriber::spawn_connection_writer(writer_half, 1024);

    let subscriber_id = SubscriberId::from(peer.to_string());
    let subscriber = Subscriber::new(subscriber_id.clone(), writer_tx.clone());

    let mut len_buf = [0u8; 4];
    let mut cmd_buf = BytesMut::with_capacity(4096);
    let mut frame_buf = BytesMut::with_capacity(1024); // Reused for SubAck
    let mut subscriptions = Vec::new();

    loop {
        // Read length-prefixed command
        let bytes_read = reader.read_exact(&mut len_buf).await;
        if let Err(e) = bytes_read {
            info!("Client {} disconnected during length read: {}", peer, e);
            break;
        }
        let len = u32::from_be_bytes(len_buf) as usize;

        // Prevent excessively large allocations or potential attacks
        if len > CONFIG.server.max_message_size_bytes {
            warn!(
                "Client {} sent oversized message ({} bytes), disconnecting.",
                peer, len
            );
            break;
        }

        cmd_buf.clear();
        cmd_buf.resize(len, 0);
        let bytes_read = reader.read_exact(&mut cmd_buf[..]).await;
        if let Err(e) = bytes_read {
            info!("Client {} disconnected during command read: {}", peer, e);
            break;
        }

        let cmd: ClientCommand = match decode_command(&cmd_buf[..]) {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "Client {} sent malformed command: {:?}, disconnecting.",
                    peer, e
                );
                break; // Disconnect client on malformed command
            }
        };

        match cmd.action {
            Action::Pub => {
                if let Some(topic) = registry.get_topic(&cmd.topic) {
                    crate::metrics::inc_published(1);
                    topic
                        .publish(Arc::new(new_message_with_ttl(cmd.payload, cmd.ttl_ms)))
                        .await;
                }
            }

            Action::Sub => {
                // SubAck response
                let ack = SubAck {
                    topic: cmd.topic.clone(),
                    info: "subscribed".into(),
                };
                let frame = ServerFrame::SubAck(ack);

                frame_buf.clear();
                encode_frame_into(&frame, &mut frame_buf);
                let frame_bytes: Bytes = frame_buf.split().freeze();
                // Send SubAck via the writer channel
                if writer_tx.send(frame_bytes).await.is_err() {
                    break;
                }

                // Register subscription
                let topic = registry.create_or_get_topic(&cmd.topic);
                topic.subscribe(subscriber.clone(), CONFIG.queues.subscriber_capacity);
                subscriptions.push(cmd.topic.clone());
            }

            Action::Unsub => {
                if let Some(topic) = registry.get_topic(&cmd.topic) {
                    topic.unsubscribe(&subscriber_id);
                }
            }

            Action::Quit => {
                info!("Client {} sent Quit command.", peer);
                break;
            }
        }
    }

    // Cleanup
    for topic in subscriptions {
        if let Some(t) = registry.get_topic(&topic) {
            t.unsubscribe(&subscriber_id);
        }
    }

    Ok(())
}
