#[path = "common.rs"]
mod common;

use std::sync::Arc;
use tokio::io::{AsyncReadExt, BufWriter};
use tokio::sync::Mutex;

use blipmq::config::CONFIG;
use blipmq::core::message::{decode_message, new_message};
use blipmq::core::subscriber::{Subscriber, SubscriberId};
use blipmq::core::topics::TopicRegistry;

#[tokio::test]
async fn message_is_fanned_out_to_all_subscribers() {
    common::init_logging();

    let registry = Arc::new(TopicRegistry::new());
    let topic = registry.create_or_get_topic(&"fan".to_string());

    let (c1, mut s1) = tokio::io::duplex(1024);
    let writer1 = Arc::new(Mutex::new(BufWriter::new(c1)));
    let sub1 = Subscriber::new(SubscriberId::from("s1".to_string()), writer1);
    topic
        .subscribe(sub1, CONFIG.queues.subscriber_capacity)
        .await;

    let (c2, mut s2) = tokio::io::duplex(1024);
    let writer2 = Arc::new(Mutex::new(BufWriter::new(c2)));
    let sub2 = Subscriber::new(SubscriberId::from("s2".to_string()), writer2);
    topic
        .subscribe(sub2, CONFIG.queues.subscriber_capacity)
        .await;

    let msg = Arc::new(new_message("hello"));
    topic.publish(msg).await;

    let mut len_buf = [0u8; 4];
    s1.read_exact(&mut len_buf).await.unwrap();
    let len1 = u32::from_be_bytes(len_buf) as usize;
    let mut buf1 = vec![0u8; len1];
    s1.read_exact(&mut buf1).await.unwrap();

    let mut len_buf = [0u8; 4];
    s2.read_exact(&mut len_buf).await.unwrap();
    let len2 = u32::from_be_bytes(len_buf) as usize;
    let mut buf2 = vec![0u8; len2];
    s2.read_exact(&mut buf2).await.unwrap();

    let r1 = decode_message(&buf1).unwrap();
    let r2 = decode_message(&buf2).unwrap();

    assert_eq!(r1.payload, b"hello");
    assert_eq!(r2.payload, b"hello");
}