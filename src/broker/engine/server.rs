//! Server engine for BlipMQ broker.
//!
//! Listens for TCP connections, parses line-based client commands, and integrates
//! with the core pub/sub engine (TopicRegistry). Implements PRODUCE/SUBSCRIBE/UNSUBSCRIBE
//! operations and streams messages back to subscribed clients.

use crate::core::{
    message::{current_timestamp, encode_message, new_message, Message},
    subscriber::{Subscriber, SubscriberId},
    topics::TopicRegistry,
};
use bytes::BytesMut;
use std::sync::Arc;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc::Receiver,
    task,
};
use tracing::{debug, error, info};

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
    ttl_ms : u64,
    queue_capacity : usize
) -> anyhow::Result<()> {
    let peer = stream.peer_addr()?;
    let (read_half, mut write_half) = stream.into_split();
    let reader = BufReader::new(read_half);
    let mut lines = reader.lines();

    let subscriber_id = SubscriberId::from(peer.to_string());
    let (subscriber, mut async_rx): (Subscriber, Receiver<Arc<Message>>) =
        Subscriber::new_with_receiver_capacity(subscriber_id.clone(),queue_capacity);
    let mut subscriptions = Vec::new();
    let mut buffer = BytesMut::with_capacity(4096);

    loop {
        tokio::select! {
                line_result = lines.next_line() => {
                    let line = match line_result? {
                        Some(l) => l,
                        None => break,
                    };

                    let parts: Vec<String> = line.trim().splitn(3, ' ').map(|s| s.to_string()).collect();

                    match parts.as_slice() {
                        [cmd, topic, body] if cmd == "PUB" => {
                            let topic = topic.clone();
                            let msg = Arc::new(new_message(body.clone().into_bytes()));
                            if let Some(t) = registry.get_topic(&topic) {
                                t.publish(msg).await;
                                write_half.write_all(b"OK PUB\n").await?;
                                write_half.flush().await?;
                                debug!("Published to {}", topic);
                            } else {
                                write_half.write_all(b"ERR NO SUCH TOPIC\n").await?;
                                write_half.flush().await?;
                                debug!("Publish to unknown topic: {}", topic);
                            }
                        }
                        [cmd, topic] if cmd == "SUB" => {
                            let topic = topic.clone();
                            let topic_obj = registry.create_or_get_topic(&topic);
                            topic_obj.subscribe(subscriber.clone()).await;
                            subscriptions.push(topic.clone());
                            write_half.write_all(format!("OK SUB {topic}\n").as_bytes()).await?;
                            write_half.flush().await?;
                            debug!("{} subscribed to {}", subscriber_id, topic);
                        }
                        [cmd, topic] if cmd == "UNSUB" => {
                            let topic = topic.clone();
                            if let Some(t) = registry.get_topic(&topic) {
                                t.unsubscribe(&subscriber_id).await;
                                write_half.write_all(format!("OK UNSUB {topic}\n").as_bytes()).await?;
                                write_half.flush().await?;
                                debug!("{} unsubscribed from {}", subscriber_id, topic);
                            } else {
                                write_half.write_all(b"ERR NO SUCH TOPIC\n").await?;
                                write_half.flush().await?;
                            }
                        }
                        [cmd] if cmd == "QUIT" || cmd == "EXIT" => {
                            write_half.write_all(b"BYE\n").await?;
                            break;
                        }
                        _ => {
                            let err_msg = format!("ERR UNKNOWN COMMAND: {}\n", line.trim());
                            write_half.write_all(err_msg.as_bytes()).await?;
                            write_half.flush().await?;
                            debug!("Unknown command from {}: {}", subscriber_id, line.trim());
                        }
                    }
                }

                Some(msg) = async_rx.recv() => {
                     if current_timestamp().saturating_sub(msg.timestamp) > ttl_ms {
                        // Drop expired message
                        continue;
                    }
                    buffer.clear();

                    let encoded = encode_message(&msg);
                    let len_prefix = (encoded.len() as u32).to_be_bytes();
                    buffer.extend_from_slice(&len_prefix);
                    buffer.extend_from_slice(&encoded);

                    let mut count = 1;
                    while count < max_batch {
                        match async_rx.try_recv() {
                            Ok(next_msg) => {
                                if current_timestamp().saturating_sub(next_msg.timestamp) <= ttl_ms {
                                    let encoded = encode_message(&next_msg);
                                    let len_prefix = (encoded.len() as u32).to_be_bytes();
                                    buffer.extend_from_slice(&len_prefix);
                                    buffer.extend_from_slice(&encoded);
                                    count += 1;
                                }
                            }
                             Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                        }
                    }

                    if let Err(err) = write_half.write_all(&buffer).await {
                        error!("Write failed to {}: {:?}", subscriber_id, err);
                        break;
                    }
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
