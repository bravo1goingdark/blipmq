use std::sync::Arc;

use blipmq::core::message::new_message;
use blipmq::core::publisher::{Publisher, PublisherConfig};
use blipmq::core::subscriber::{Subscriber, SubscriberId};
use blipmq::core::topics::registry::TopicRegistry;

#[tokio::test]
async fn publish_to_existing_topic_delivers_message() {
    let registry = Arc::new(TopicRegistry::new());
    let publisher = Publisher::new(registry.clone(), PublisherConfig::default());

    let topic_name = "test-topic".to_string();
    let topic = registry.create_or_get_topic(&topic_name);

    let subscriber = Subscriber::new(SubscriberId::from("sub1"));
    topic.subscribe(subscriber.clone()).await;

    let payload = "hello";
    let msg = Arc::new(new_message(payload));
    publisher.publish(&topic_name, msg.clone()).await;

    let received = subscriber.receiver().try_recv().expect("no message");
    assert_eq!(received.payload, payload.as_bytes());
}

#[tokio::test]
async fn publish_to_nonexistent_topic_is_dropped() {
    let registry = Arc::new(TopicRegistry::new());
    let publisher = Publisher::new(registry.clone(), PublisherConfig::default());

    let topic_a = registry.create_or_get_topic(&"a".to_string());
    let subscriber = Subscriber::new(SubscriberId::from("s1"));
    topic_a.subscribe(subscriber.clone()).await;

    let msg = Arc::new(new_message("ignored"));
    publisher.publish(&"b".to_string(), msg).await;

    assert!(subscriber.receiver().try_recv().is_err());
}
