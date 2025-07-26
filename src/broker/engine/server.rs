//! Server engine for BlipMQ broker.
//!
//! Listens for TCP connections using a length-prefixed Protobuf command protocol
//! and integrates with the core pub/sub engine (TopicRegistry). Implements
//! PRODUCE/SUBSCRIBE/UNSUBSCRIBE operations and streams messages back to
//! subscribed clients.

use crate::core::{
    command::{decode_command, Action, ClientCommand},
    message::{current_timestamp, encode_message, new_message, Message},
    subscriber::{Subscriber, SubscriberId},
    topics::TopicRegistry,
};
use bytes::BytesMut;
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufWriter},
    net::{TcpListener, TcpStream},
    sync::mpsc::Receiver,
    task,
    time::{Duration, Instant},
};
use tracing::{error, info};

/// Starts the BlipMQ broker server.
pub async fn serve(
    bind_addr: &str,
    max_batch: usize,
    ttl_ms: u64,
    queue_capacity: usize,
) -> anyhow::Result<()> {
    info!("Starting BlipMQ broker on {}", bind_addr);

    let listener = TcpListener::bind(bind_addr).await?;
    let registry = Arc::new(TopicRegistry::new());

    loop {
        let (socket, peer) = listener.accept().await?;
        let registry = Arc::clone(&registry);
        info!("Client connected: {}", peer);

        task::spawn(handle_client(
            socket,
            registry,
            max_batch,
            ttl_ms,
            queue_capacity,
        ));
    }
}

async fn handle_client(
    stream: TcpStream,
    registry: Arc<TopicRegistry>,
    max_batch: usize,
    ttl_ms: u64,
    queue_capacity: usize,
) -> anyhow::Result<()> {
    let peer = stream.peer_addr()?;
    let (mut reader, writer) = stream.into_split();
    let mut writer = BufWriter::new(writer);

    let subscriber_id = SubscriberId::from(peer.to_string());
    let (subscriber, mut async_rx): (Subscriber, Receiver<Arc<Message>>) =
        Subscriber::new_with_receiver_capacity(subscriber_id.clone(), queue_capacity);
    let mut subscriptions = Vec::new();
    let mut len_buf = [0u8; 4];
    let mut msg_buf = Vec::with_capacity(4096);
    let mut send_buf = BytesMut::with_capacity(8192);

    loop {
        tokio::select! {
            read_res = reader.read_exact(&mut len_buf) => {
                if read_res.is_err() {
                    break;
                }
                let msg_len = u32::from_be_bytes(len_buf) as usize;
                msg_buf.resize(msg_len, 0);
                if reader.read_exact(&mut msg_buf).await.is_err() {
                    break;
                }

                let cmd: ClientCommand = match decode_command(&msg_buf) {
                    Ok(c) => c,
                    Err(_) => {
                        writer.write_all(b"ERR MALFORMED\n").await?;
                        writer.flush().await?;
                        continue;
                    }
                };

                match Action::try_from(cmd.action).ok() {
                    Some(Action::Pub) => {
                        let topic = cmd.topic;
                        let msg = Arc::new(new_message(cmd.payload));
                        if let Some(t) = registry.get_topic(&topic) {
                            t.publish(msg).await;
                            writer.write_all(b"OK PUB\n").await?;
                            writer.flush().await?;
                        } else {
                            writer.write_all(b"ERR NO SUCH TOPIC\n").await?;
                            writer.flush().await?;
                        }
                    }
                    Some(Action::Sub) => {
                        let topic = cmd.topic;
                        let topic_obj = registry.create_or_get_topic(&topic);
                        topic_obj.subscribe(subscriber.clone()).await;
                        subscriptions.push(topic.clone());
                        writer.write_all(format!("OK SUB {topic}\n").as_bytes()).await?;
                        writer.flush().await?;
                    }
                    Some(Action::Unsub) => {
                        let topic = cmd.topic;
                        if let Some(t) = registry.get_topic(&topic) {
                            t.unsubscribe(&subscriber_id).await;
                            writer.write_all(format!("OK UNSUB {topic}\n").as_bytes()).await?;
                            writer.flush().await?;
                        } else {
                            writer.write_all(b"ERR NO SUCH TOPIC\n").await?;
                            writer.flush().await?;
                        }
                    }
                    Some(Action::Quit) | None => {
                        writer.write_all(b"BYE\n").await?;
                        writer.flush().await?;
                        break;
                    }
                }
            }

            Some(msg) = async_rx.recv() => {
                if current_timestamp().saturating_sub(msg.timestamp) > ttl_ms {
                    continue;
                }

                send_buf.clear();
                let encoded = encode_message(&msg);
                send_buf.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
                send_buf.extend_from_slice(&encoded);

                let mut count = 1;
                let deadline = Instant::now() + Duration::from_micros(50);
                while count < max_batch && Instant::now() < deadline {
                    match async_rx.try_recv() {
                        Ok(next_msg) => {
                            if current_timestamp().saturating_sub(next_msg.timestamp) <= ttl_ms {
                                let enc = encode_message(&next_msg);
                                send_buf.extend_from_slice(&(enc.len() as u32).to_be_bytes());
                                send_buf.extend_from_slice(&enc);
                                count += 1;
                            }
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                    }
                }

                if let Err(e) = writer.write_all(&send_buf).await {
                    error!("Write failed to {}: {:?}", subscriber_id, e);
                    break;
                }
                writer.flush().await?;
            }
        }
    }

    for topic in subscriptions {
        if let Some(t) = registry.get_topic(&topic) {
            t.unsubscribe(&subscriber_id).await;
        }
    }

    Ok(())
}
