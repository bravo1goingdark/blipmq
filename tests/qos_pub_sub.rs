use std::sync::Arc;

use blipmq::config::CONFIG;
use std::sync::Once;

fn init_logging() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        blipmq::logging::init_logging();
    });
}
use blipmq::core::message::{decode_message, new_message};
use blipmq::core::publisher::{Publisher, PublisherConfig};
use blipmq::core::subscriber::{Subscriber, SubscriberId};
use blipmq::core::topics::registry::TopicRegistry;
use tokio::io::{AsyncReadExt, BufWriter};
use tokio::sync::Mutex;

#[tokio::test]
async fn publish_to_existing_topic_delivers_message() {
    init_logging();
    let registry = Arc::new(TopicRegistry::new());
    let publisher = Publisher::new(registry.clone(), PublisherConfig::default());

    let topic_name = "test-topic".to_string();
    let topic = registry.create_or_get_topic(&topic_name);

    let (client, mut server) = tokio::io::duplex(1024);
    let writer = Arc::new(Mutex::new(BufWriter::new(client)));
    let subscriber = Subscriber::new(SubscriberId::from("sub1".to_string()), writer);
    topic
        .subscribe(subscriber.clone(), CONFIG.queues.subscriber_capacity)
        .await;

    let payload = "hello";
    let msg = Arc::new(new_message(payload));
    publisher.publish(&topic_name, msg.clone()).await;

    let mut len_buf = [0u8; 4];
    server.read_exact(&mut len_buf).await.unwrap();
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    server.read_exact(&mut buf).await.unwrap();
    let received = decode_message(&buf).unwrap();
    assert_eq!(received.payload, payload.as_bytes());
}

#[tokio::test]
async fn publish_to_nonexistent_topic_is_dropped() {
    init_logging();
    let registry = Arc::new(TopicRegistry::new());
    let publisher = Publisher::new(registry.clone(), PublisherConfig::default());

    let topic_a = registry.create_or_get_topic(&"a".to_string());
    let (client, mut server) = tokio::io::duplex(1024);
    let writer = Arc::new(Mutex::new(BufWriter::new(client)));
    let subscriber = Subscriber::new(SubscriberId::from("s1".to_string()), writer);
    topic_a
        .subscribe(subscriber.clone(), CONFIG.queues.subscriber_capacity)
        .await;

    let msg = Arc::new(new_message("ignored"));
    publisher.publish(&"b".to_string(), msg).await;

    use tokio::time::{timeout, Duration};
    let mut len_buf = [0u8; 4];
    let read_res = timeout(Duration::from_millis(100), server.read_exact(&mut len_buf)).await;
    assert!(read_res.is_err());
}
