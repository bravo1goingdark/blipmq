use blipmq::core::delivery_mode::DeliveryMode;
use blipmq::core::message::Message;
use blipmq::core::publisher::{Publisher, PublisherConfig};
use blipmq::core::subscriber::{Subscriber, SubscriberId};
use blipmq::core::topics::registry::TopicRegistry;

use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::Instant;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn stress_qos0_scaled() {
    const NUM_SUBSCRIBERS: usize = 100;
    const NUM_MESSAGES: usize = 1_000_000;

    let topic_name = "scale/test".to_string();
    let registry = Arc::new(TopicRegistry::new());
    let publisher = Publisher::new(
        Arc::clone(&registry),
        PublisherConfig {
            delivery_mode: DeliveryMode::Ordered,
        },
    );

    let topic = registry.create_or_get_topic(&topic_name);

    // Create subscribers
    let mut receivers = Vec::new();
    for i in 0..NUM_SUBSCRIBERS {
        let (tx, rx) = mpsc::unbounded_channel();
        let sub = Subscriber::new(SubscriberId::from(format!("sub-{}", i)), tx);
        topic.subscribe(sub).await;
        receivers.push(rx);
    }

    // Publish messages
    let start = Instant::now();
    for i in 0..NUM_MESSAGES {
        let msg = Arc::new(Message::new(format!("msg-{i}")));
        publisher.publish(&topic_name, msg).await;
    }
    let duration = start.elapsed();

    let total_fanout = NUM_MESSAGES * NUM_SUBSCRIBERS;
    println!("\n✅ Sent {NUM_MESSAGES} messages to {NUM_SUBSCRIBERS} subscribers in {duration:?}");
    println!(
        "🚀 Approx throughput: {:.2} messages/sec (fanout total: {total_fanout})",
        total_fanout as f64 / duration.as_secs_f64(),
    );

    // Drain (optional: slow step for large counts, skip if not needed)
    let mut total_received = 0;
    for mut rx in receivers {
        while let Ok(Some(_)) = tokio::time::timeout(tokio::time::Duration::from_millis(2), rx.recv()).await {
            total_received += 1;
        }
    }

    println!("📬 Total received: {total_received} (QoS 0 may drop)");
}
