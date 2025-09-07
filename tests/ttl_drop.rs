use std::sync::Arc;

use blipmq::core::message::{current_timestamp, decode_frame, with_custom_message, ServerFrame};
use blipmq::core::subscriber::{Subscriber, SubscriberId};
use blipmq::core::topics::registry::TopicRegistry;
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn expired_message_is_dropped() {
    // Setup topic and one subscriber
    let registry = Arc::new(TopicRegistry::new());
    let topic_name = "ttl".to_string();
    let topic = registry.create_or_get_topic(&topic_name);

    let (client, mut server) = tokio::io::duplex(1024);
    let tx = blipmq::core::subscriber::spawn_connection_writer(client, 1024);
    let subscriber = Subscriber::new(SubscriberId::from("sub_ttl".to_string()), tx);
    topic
        .subscribe(subscriber, blipmq::config::CONFIG.queues.subscriber_capacity);

    // Create an already-expired message: ts + ttl <= now
    let now = current_timestamp();
    let ts = now.saturating_sub(50);
    let msg = with_custom_message(123, "expired", ts, 10);

    topic.publish(Arc::new(msg)).await;

    use tokio::time::{timeout, Duration};
    // Expect no frame to arrive (channel should be idle) within timeout
    let mut len_buf = [0u8; 4];
    let res = timeout(Duration::from_millis(100), server.read_exact(&mut len_buf)).await;
    assert!(res.is_err(), "expired message should not be delivered");
}

