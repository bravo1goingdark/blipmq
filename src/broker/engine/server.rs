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
use tokio::io::AsyncWriteExt;
use tokio::{
    io::{AsyncReadExt, BufReader, BufWriter},
    net::{TcpListener, TcpStream},
    sync::Mutex,
    task,
};
use tracing::{error, info, warn};

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
                error!("Error handling client {}: {:?}", peer_addr, e);
            } else {
                info!("Client disconnected: {}", peer_addr);
            }
        });
    }
}

async fn handle_client(stream: TcpStream, registry: Arc<TopicRegistry>) -> anyhow::Result<()> {
    let peer = stream.peer_addr()?;
    let (reader_half, writer_half) = stream.into_split();
    let mut reader = BufReader::new(reader_half);

    let shared_writer = Arc::new(Mutex::new(BufWriter::new(writer_half)));

    let subscriber_id = SubscriberId::from(peer.to_string());
    let subscriber = Subscriber::new(subscriber_id.clone(), shared_writer.clone());

    let mut len_buf = [0u8; 4];
    let mut cmd_buf = Vec::with_capacity(4096);
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
            warn!("Client {} sent oversized message ({} bytes), disconnecting.", peer, len);
            break;
        }

        cmd_buf.clear();
        cmd_buf.resize(len, 0);
        let bytes_read = reader.read_exact(&mut cmd_buf).await;
        if let Err(e) = bytes_read {
            info!("Client {} disconnected during command read: {}", peer, e);
            break;
        }

        let cmd: ClientCommand = match decode_command(&cmd_buf) {
            Ok(c) => c,
            Err(e) => {
                warn!("Client {} sent malformed command: {:?}, disconnecting.", peer, e);
                break; // Disconnect client on malformed command
            }
        };

        match cmd.action {
            Action::Pub => {
                if let Some(topic) = registry.get_topic(&cmd.topic) {
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
                {
                    let mut w = shared_writer.lock().await;
                    w.write_all(&frame_buf).await?;
                    w.flush().await?;
                }

                // Register subscription
                let topic = registry.create_or_get_topic(&cmd.topic);
                topic
                    .subscribe(subscriber.clone(), CONFIG.queues.subscriber_capacity)
                    .await;
                subscriptions.push(cmd.topic.clone());
            }

            Action::Unsub => {
                if let Some(topic) = registry.get_topic(&cmd.topic) {
                    topic.unsubscribe(&subscriber_id).await;
                }
            }

            Action::Quit => {
                info!("Client {} sent Quit command.", peer);
                break;
            },
        }
    }

    // Cleanup
    for topic in subscriptions {
        if let Some(t) = registry.get_topic(&topic) {
            t.unsubscribe(&subscriber_id).await;
        }
    }

    Ok(())
}
