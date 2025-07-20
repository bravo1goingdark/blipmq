//! Server engine for BlipMQ broker.
//!
//! Listens for TCP connections, parses line-based client commands, and integrates
//! with the core pub/sub engine (TopicRegistry). Implements PRODUCE/SUBSCRIBE/UNSUBSCRIBE
//! operations and streams messages back to subscribed clients.

use std::sync::Arc;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task,
};
use tracing::{debug, error, info};

use crate::core::{
    topics::TopicRegistry,
    message::Message,
    subscriber::{Subscriber, SubscriberId},
};

/// Starts the BlipMQ broker server.
///
/// Binds to the specified address (e.g. "127.0.0.1:6379"), accepts incoming client
/// connections, and spawns a handler task per connection.
///
/// # Example
///
/// ```no_run
/// use self::blipmq::broker::serve;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     tracing_subscriber::fmt::init();
///     serve("0.0.0.0:6379").await
/// }
/// ```
pub async fn serve(addr: &str) -> anyhow::Result<()> {
    info!("Starting BlipMQ broker on {}", addr);
    let listener: TcpListener = TcpListener::bind(addr).await?;
    let registry: Arc<TopicRegistry> = Arc::new(TopicRegistry::new());

    loop {
        let (socket, peer) = listener.accept().await?;
        let registry = Arc::clone(&registry);
        info!("Client connected: {}", peer);

        task::spawn(async move {
            if let Err(err) = handle_client(socket, registry).await {
                error!("Error handling client {}: {:?}", peer, err);
            } else {
                debug!("Client {} disconnected", peer);
            }
        });
    }
}

/// Handles an individual client connection.
///
/// Supports the following commands:
/// - `PUB <topic> <message>`: publish a message to `topic`
/// - `SUB <topic>`: subscribe this client to `topic`
/// - `UNSUB <topic>`: unsubscribe this client from `topic`
///
/// For subscribers, messages are pushed back on the same connection:
/// `MSG <id> <payload>` per line.
async fn handle_client(stream: TcpStream, registry: Arc<TopicRegistry>) -> anyhow::Result<()> {
    let peer = stream.peer_addr()?;
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    // Create a channel for this subscriber to receive messages
    let (tx, mut rx) = mpsc::unbounded_channel::<Arc<Message>>();
    let subscriber_id = SubscriberId::from(peer.to_string());
    let subscriber = Subscriber::new(subscriber_id.clone(), tx);

    // Track subscribed topics for cleanup on disconnect
    let mut subscriptions = Vec::new();

    loop {
        tokio::select! {
            // Read next client command
            line = lines.next_line() => {
                let line = match line? {
                    Some(l) => l,
                    None => break, // client disconnected
                };

                // Parse and execute command
                let parts: Vec<_> = line.trim().splitn(3, ' ').collect();
                match parts.as_slice() {
                    ["PUB", topic, body] => {
                        let msg = Arc::new(Message::new(body.as_bytes().to_vec()));
                        if let Some(t) = registry.get_topic(&topic.to_string()) {
                            t.publish(msg).await;
                            debug!("Published to {}", topic);
                        } else {
                            debug!("Publish to unknown topic: {}", topic);
                        }
                    }
                    ["SUB", topic] => {
                        let topic_obj = registry.create_or_get_topic(&topic.to_string());
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
            // Send any queued messages to subscriber
            Some(msg) = rx.recv() => {
                let payload = String::from_utf8_lossy(&msg.payload);
                write_half
                    .write_all(format!("MSG {} {}\n", msg.id, payload).as_bytes())
                    .await?;
            }
        }
    }

    // Cleanup subscriptions on client disconnect
    for topic in subscriptions {
        if let Some(t) = registry.get_topic(&topic) {
            t.unsubscribe(&subscriber_id).await;
        }
    }

    Ok(())
}
