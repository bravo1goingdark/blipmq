//! Server engine for BlipMQ broker.
//!
//! Uses length-prefixed Protobuf frames for both SUB-ACK and published messages.

use crate::config::CONFIG;
use crate::core::message::proto::server_frame::Body as FrameBody;
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
use tracing::{error, info};

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

    let shared_writer = Arc::new(Mutex::new(BufWriter::new(writer_half)));

    // Subscriber flush task
    let subscriber_id = SubscriberId::from(peer.to_string());
    let subscriber = Subscriber::new(subscriber_id.clone(), shared_writer.clone());

    let mut len_buf = [0u8; 4];
    let mut cmd_buf = Vec::with_capacity(4096);
    let mut subscriptions = Vec::new();

    loop {
        // Read length-prefixed command
        if reader.read_exact(&mut len_buf).await.is_err() {
            break;
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        cmd_buf.resize(len, 0);
        if reader.read_exact(&mut cmd_buf).await.is_err() {
            break;
        }

        let cmd: ClientCommand = match decode_command(&cmd_buf) {
            Ok(c) => c,
            Err(_) => continue,
        };

        match Action::try_from(cmd.action).ok() {
            Some(Action::Pub) => {
                if let Some(topic) = registry.get_topic(&cmd.topic) {
                    topic
                        .publish(Arc::new(new_message_with_ttl(cmd.payload, cmd.ttl_ms)))
                        .await;
                }
            }

            Some(Action::Sub) => {
                // Send SubAck frame
                let ack = SubAck {
                    topic: cmd.topic.clone(),
                    info: "subscribed".into(),
                };
                let frame = ServerFrame {
                    body: Some(FrameBody::SubAck(ack)),
                };
                let mut buf = BytesMut::new();
                encode_frame_into(&frame, &mut buf);
                {
                    let mut w = shared_writer.lock().await;
                    w.write_all(&buf).await?;
                    w.flush().await?;
                }

                // Register subscription
                let t = registry.create_or_get_topic(&cmd.topic);
                t.subscribe(subscriber.clone(), CONFIG.queues.subscriber_capacity)
                    .await;
                subscriptions.push(cmd.topic.clone());
            }

            Some(Action::Unsub) => {
                if let Some(topic) = registry.get_topic(&cmd.topic) {
                    topic.unsubscribe(&subscriber_id).await;
                }
            }

            Some(Action::Quit) | None => break,
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
