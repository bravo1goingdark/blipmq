use blipmq::core::message::Message;
use blipmq::core::publisher::{Publisher, PublisherConfig};
use blipmq::core::subscriber::{Subscriber, SubscriberId};
use blipmq::core::topics::registry::TopicRegistry;
use blipmq::core::delivery_mode::DeliveryMode;

use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

#[tokio::test]
async fn qos0_ordered_pubsub_works() {
    let registry = Arc::new(TopicRegistry::new());
    let publisher = Publisher::new(registry.clone(), PublisherConfig {
        delivery_mode: DeliveryMode::Ordered,
    });

    let topic_name = "test/topic".to_string();
    let topic = registry.create_or_get_topic(&topic_name);

    // Create a subscriber and channel
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sub_id = SubscriberId::from("sub-1");
    let subscriber = Subscriber::new(sub_id.clone(), tx);

    topic.subscribe(subscriber).await;

    let msg = Arc::new(Message::new("hello world"));
    publisher.publish(&topic_name, msg.clone()).await;

    // Wait up to 100ms for the message
    let received = timeout(Duration::from_millis(100), rx.recv()).await.unwrap();

    assert_eq!(received.unwrap().payload, b"hello world");
}
