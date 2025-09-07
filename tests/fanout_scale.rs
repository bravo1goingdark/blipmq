use std::sync::Arc;

use blipmq::core::message::{decode_frame, new_message, ServerFrame};
use blipmq::core::subscriber::{Subscriber, SubscriberId};
use blipmq::core::topics::registry::TopicRegistry;
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn fanout_to_many_subscribers() {
    let registry = Arc::new(TopicRegistry::new());
    let topic = registry.create_or_get_topic(&"scale".to_string());

    // Create a moderate number of subscribers
    let n = 64usize;
    let mut readers = Vec::with_capacity(n);
    for i in 0..n {
        let (client, server) = tokio::io::duplex(2048);
        let tx = blipmq::core::subscriber::spawn_connection_writer(client, 1024);
        let sub = Subscriber::new(SubscriberId::from(format!("s{i}")), tx);
        topic
            .subscribe(sub, blipmq::config::CONFIG.queues.subscriber_capacity)
            .await;
        readers.push(server);
    }

    let payload = "scale-test";
    let msg = Arc::new(new_message(payload));
    topic.publish(msg).await;

    // Each reader should get exactly one frame; validate content
    for mut r in readers {
        let mut len_buf = [0u8; 4];
        r.read_exact(&mut len_buf).await.unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf).await.unwrap();
        match decode_frame(&buf).unwrap() {
            ServerFrame::Message(m) => assert_eq!(m.payload, payload.as_bytes()),
            _ => panic!("unexpected frame type"),
        }
    }
}

