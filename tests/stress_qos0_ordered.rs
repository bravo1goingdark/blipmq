use blipmq::core::delivery_mode::DeliveryMode;
use blipmq::core::message::Message;
use blipmq::core::publisher::{Publisher, PublisherConfig};
use blipmq::core::subscriber::{Subscriber, SubscriberId};
use blipmq::core::topics::registry::TopicRegistry;

use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::Instant;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_qos0_throughput() {
    const NUM_SUBSCRIBERS: usize = 10;
    const NUM_MESSAGES: usize = 100_000;

    let topic_name = "stress/test".to_string();
    let registry = Arc::new(TopicRegistry::new());
    let publisher = Publisher::new(
        Arc::clone(&registry),
        PublisherConfig {
            delivery_mode: DeliveryMode::Unordered,
        },
    );

    let topic = registry.create_or_get_topic(&topic_name);

    // Setup subscribers and message counters
    let mut receivers = Vec::new();
    for i in 0..NUM_SUBSCRIBERS {
        let (tx, rx) = mpsc::unbounded_channel();
        let id = SubscriberId::from(format!("sub-{}", i));
        let sub = Subscriber::new(id, tx);
        topic.subscribe(sub).await;
        receivers.push(rx);
    }

    // Publish messages
    let start = Instant::now();
    for i in 0..NUM_MESSAGES {
        let msg = Arc::new(Message::new(format!("msg-{}", i)));
        publisher.publish(&topic_name, msg).await;
    }
    let duration = start.elapsed();

    let msgs_sent = NUM_MESSAGES;
    let msgs_expected = NUM_MESSAGES * NUM_SUBSCRIBERS;
    println!(
        "\n✅ Sent {msgs_sent} messages to {NUM_SUBSCRIBERS} subscribers in {:?}",
        duration
    );
    println!(
        "🚀 Approx throughput: {:.2} messages/sec (total fanout: {})",
        msgs_expected as f64 / duration.as_secs_f64(),
        msgs_expected
    );

    // (Optional) drain and count messages received
    let mut total_received = 0;
    for mut rx in receivers {
        while let Ok(Some(_)) = tokio::time::timeout(tokio::time::Duration::from_millis(1), rx.recv()).await {
            total_received += 1;
        }
    }

    println!("📬 Total received: {} (may drop in QoS 0)", total_received);
}
