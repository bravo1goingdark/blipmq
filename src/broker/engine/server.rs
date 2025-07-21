//! Server engine for BlipMQ broker.
//!
//! Listens for TCP connections, parses line-based client commands, and integrates
//! with the core pub/sub engine (TopicRegistry). Implements PRODUCE/SUBSCRIBE/UNSUBSCRIBE
//! operations and streams messages back to subscribed clients.

use crate::core::topics::Topic;
use crate::{
    config::load_config,
    core::{
        message::Message,
        subscriber::{Subscriber, SubscriberId},
        topics::TopicRegistry,
    },
    Config,
};
use bytes::BytesMut;
use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task,
};
use tracing::{debug, error, info};

/// Starts the BlipMQ broker server.
///
/// Loads `blipmq.toml`, binds to `server.bind_addr`, and spawns a handler task per connection.
pub async fn serve() -> anyhow::Result<()> {
    // Load configuration
    let cfg: Config = load_config("blipmq.toml")?;
    info!("Starting BlipMQ broker on {}", cfg.server.bind_addr);

    let listener: TcpListener = TcpListener::bind(&cfg.server.bind_addr).await?;
    let registry: Arc<TopicRegistry> = Arc::new(TopicRegistry::new());

    loop {
        let (socket, peer) = listener.accept().await?;
        let registry: Arc<TopicRegistry> = Arc::clone(&registry);
        let max_batch: usize = cfg.delivery.max_batch;
        info!("Client connected: {}", peer);

        task::spawn(async move {
            if let Err(err) = handle_client(socket, registry, max_batch).await {
                error!("Error handling client {}: {:?}", peer, err);
            } else {
                debug!("Client {} disconnected", peer);
            }
        });
    }
}

/// Handles an individual client connection.
///
/// Supports:
/// - `PUB <topic> <message>`
/// - `SUB <topic>`
/// - `UNSUB <topic>`
///
/// For subscribers, messages are pushed back as:
/// `MSG <id> <payload>\n`, batched up to `max_batch`.
async fn handle_client(
    stream: TcpStream,
    registry: Arc<TopicRegistry>,
    max_batch: usize,
) -> anyhow::Result<()> {
    let peer: SocketAddr = stream.peer_addr()?;
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    // Channel for this subscriber
    let (tx, mut rx) = mpsc::unbounded_channel::<Arc<Message>>();
    let subscriber_id: SubscriberId = SubscriberId::from(peer.to_string());
    let subscriber: Subscriber = Subscriber::new(subscriber_id.clone(), tx);

    // Track topics to unsubscribe on disconnect
    let mut subscriptions: Vec<String> = Vec::new();

    loop {
        tokio::select! {
            // Client command
            line = lines.next_line() => {
                let line : String = match line? {
                    Some(l) => l,
                    None => break,
                };

                let parts: Vec<_> = line.trim().splitn(3, ' ').collect();
                match parts.as_slice() {
                    ["PUB", topic, body] => {
                        let msg : Arc<Message> = Arc::new(Message::new(body.as_bytes().to_vec()));
                        if let Some(t) = registry.get_topic(&topic.to_string()) {
                            t.publish(msg).await;
                            debug!("Published to {}", topic);
                        } else {
                            debug!("Publish to unknown topic: {}", topic);
                        }
                    }
                    ["SUB", topic] => {
                        let topic_obj : Arc<Topic> = registry.create_or_get_topic(&topic.to_string());
                        topic_obj.subscribe(subscriber.clone()).await;
                        subscriptions.push(topic.to_string());
                        write_half
                            .write_all(format!("OK SUB {}\n", topic).as_bytes())
                            .await?;
                        debug!("{} subscribed to {}", subscriber_id, topic);
                    }
                    ["UNSUB", topic] => {
                        if let Some(t) = registry.get_topic(&topic.to_string()) {
                            t.unsubscribe(&subscriber_id).await;
                            write_half
                                .write_all(format!("OK UNSUB {}\n", topic).as_bytes())
                                .await?;
                            debug!("{} unsubscribed from {}", subscriber_id, topic);
                        }
                    }
                    cmd => {
                        write_half
                            .write_all(format!("ERR UNKNOWN COMMAND: {:?}\n", cmd).as_bytes())
                            .await?;
                        debug!("Unknown command from {}: {:?}", subscriber_id, cmd);
                    }
                }
            }

            // Batched subscriber flush
            Some(msg) = rx.recv() => {
                let mut buffer : BytesMut = BytesMut::with_capacity(4096);
                let payload: Cow<str> = String::from_utf8_lossy(&msg.payload);
                buffer.extend_from_slice(format!("MSG {} {}\n", msg.id, payload).as_bytes());

                let mut count : usize = 1;
                while let Ok(next_msg) = rx.try_recv() {
                    if count >= max_batch {
                        break;
                    }
                    let payload: Cow<str> = String::from_utf8_lossy(&next_msg.payload);
                    buffer.extend_from_slice(format!("MSG {} {}\n", next_msg.id, payload).as_bytes());
                    count += 1;
                }

                if let Err(err) = write_half.write_all(&buffer).await {
                    error!("Write failed to {}: {:?}", subscriber_id, err);
                    break;
                }
            }
        }
    }

    // Unsubscribe on disconnect
    for topic in subscriptions {
        if let Some(t) = registry.get_topic(&topic) {
            t.unsubscribe(&subscriber_id).await;
        }
    }

    Ok(())
}
