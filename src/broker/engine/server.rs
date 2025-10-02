//! Server engine for BlipMQ broker.
//!
//! Uses length-prefixed FlatBuffers for all protocol messages.

use crate::config::CONFIG;
use crate::core::{
    auth::AuthManager,
    command::{decode_command, Action, ClientCommand},
    message::{encode_frame_into, new_message_with_ttl, ServerFrame, SubAck},
    subscriber::{Subscriber, SubscriberId},
    topics::TopicRegistry,
};

use bytes::Bytes;
use crate::core::memory_pool::convenience::get_medium_buffer;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
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
    let auth_manager = Arc::new(AuthManager::new("blipmq-secret-change-in-production".to_string()));
    let connection_count = Arc::new(AtomicUsize::new(0));

    // Spawn a very small HTTP metrics server on CONFIG.metrics.bind_addr
    let metrics_addr = CONFIG.metrics.bind_addr.clone();
    tokio::spawn(async move {
        if let Err(e) = serve_metrics(&metrics_addr).await {
            warn!("metrics server error: {:?}", e);
        }
    });

    let shutdown_signal = tokio::signal::ctrl_c();
    tokio::pin!(shutdown_signal);
    
    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (mut socket, peer_addr) = accept_result?;
        
        // Check connection limit
        let current_connections = connection_count.load(Ordering::Relaxed);
        if current_connections >= CONFIG.server.max_connections {
            warn!("Connection limit reached ({}), rejecting {}", CONFIG.server.max_connections, peer_addr);
            let _ = socket.shutdown();
            continue;
        }
        
        socket.set_nodelay(true)?;
        let registry = Arc::clone(&registry);
        let auth_manager = Arc::clone(&auth_manager);
        let conn_count = Arc::clone(&connection_count);
        info!("Client connected: {}", peer_addr);

        // Increment connection count
        conn_count.fetch_add(1, Ordering::Relaxed);

        task::spawn(async move {
            let result = handle_client(socket, registry, auth_manager).await;
            
            // Decrement connection count on disconnect
            conn_count.fetch_sub(1, Ordering::Relaxed);
            
            if let Err(e) = result {
                error!("Error handling client {}: {:?}", peer_addr, e);
            } else {
                info!("Client disconnected: {}", peer_addr);
            }
        });
            }
            _ = &mut shutdown_signal => {
                info!("Shutdown signal received, stopping broker...");
                break;
            }
        }
    }
    
    info!("BlipMQ broker shutdown complete");
    Ok(())
}
async fn serve_metrics(addr: &str) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let listener = TcpListener::bind(addr).await?;
    info!("metrics listening on {}", addr);
    loop {
        let (mut sock, _peer) = listener.accept().await?;
        let body = crate::metrics::snapshot();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
            body.len(), body
        );
        if let Err(e) = sock.write_all(resp.as_bytes()).await {
            warn!("metrics write error: {:?}", e);
        }
    }
}

async fn handle_client(
    stream: TcpStream,
    registry: Arc<TopicRegistry>,
    _auth_manager: Arc<AuthManager>, // Prefixed with _ until fully integrated
) -> anyhow::Result<()> {
    let peer = stream.peer_addr()?;
    let (reader_half, writer_half) = stream.into_split();
    let mut reader = BufReader::new(reader_half);

    // Per-connection writer task: all frames go through this channel
    let writer_tx = crate::core::subscriber::spawn_connection_writer(writer_half, 1024);

    let subscriber_id = SubscriberId::from(peer.to_string());
    let subscriber = Subscriber::new(subscriber_id.clone(), writer_tx.clone());

    let mut len_buf = [0u8; 4];
    let mut cmd_buf = get_medium_buffer(); // Use pooled buffer
    let mut frame_buf = get_medium_buffer(); // Use pooled buffer for SubAck
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
        
        // Additional validation: prevent zero-length and malformed requests
        if len == 0 {
            warn!("Client {} sent empty message, disconnecting.", peer);
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

        // Validate topic name
        if cmd.topic.is_empty() || cmd.topic.len() > 256 || cmd.topic.contains(['/', '\0', '\n', '\r']) {
            warn!("Client {} sent invalid topic name: {}", peer, cmd.topic);
            break;
        }
        
        // TODO: Extract API key from command or connection headers for auth
        // For now, we'll use a default auth context for unauthenticated connections
        let auth_context: Option<crate::core::auth::AuthContext> = None; // Replace with actual auth extraction logic
        
        match cmd.action {
            Action::Pub => {
                // Additional payload validation
                if cmd.payload.len() > CONFIG.server.max_message_size_bytes {
                    warn!("Client {} sent oversized payload ({} bytes)", peer, cmd.payload.len());
                    break;
                }
                
                // TODO: Add authentication and authorization checks here
                // For now, allow all publish operations
                
                if let Some(topic) = registry.get_topic(&cmd.topic) {
                    crate::metrics::inc_published(1);
                    topic
                        .publish(Arc::new(new_message_with_ttl(cmd.payload, cmd.ttl_ms)))
                        .await;
                }
            }

            Action::Sub => {
                // TODO: Add authentication and authorization checks here
                // For now, allow all subscribe operations
                
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
