use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use blipmq_chaos::simulate_crash;
use blipmq_core::{Broker, BrokerConfig, ClientId, QoSLevel, TopicName};
use blipmq_wal::WriteAheadLog;
use bytes::Bytes;

fn wal_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("blipmq_chaos_test_{}.wal", name));
    let _ = std::fs::remove_file(&path);
    path
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chaos_crash_recovery_qos1() {
    let path = wal_path("qos1_recovery");

    let wal = Arc::new(
        WriteAheadLog::open(&path)
            .await
            .expect("failed to open WAL"),
    );

    let config = BrokerConfig {
        default_qos: QoSLevel::AtLeastOnce,
        message_ttl: Duration::from_secs(60),
        per_subscriber_queue_capacity: 1024,
        max_retries: 3,
        retry_base_delay: Duration::from_millis(50),
    };

    let broker1 = Arc::new(Broker::new_with_wal(config.clone(), wal.clone()));

    let topic = TopicName::new("chaos-topic");
    let client1 = ClientId::new("client-1");
    let sub1 = broker1.subscribe(client1, topic.clone(), QoSLevel::AtLeastOnce);

    let total_messages = 32u32;
    let mut payloads = Vec::new();
    for i in 0..total_messages {
        let payload_str = format!("msg-{}", i);
        payloads.push(payload_str.clone());
        broker1
            .publish_durable(
                &topic,
                Bytes::from(payload_str),
                QoSLevel::AtLeastOnce,
            )
            .await
            .expect("durable publish failed");
    }

    // Ack the first half, leave the rest unacked to simulate in-flight messages.
    let mut acked = HashSet::new();
    for _ in 0..(total_messages / 2) {
        if let Some(msg) = broker1.poll(sub1) {
            let tag = msg
                .delivery_tag
                .expect("QoS1 delivery should carry a tag");
            let payload_str =
                String::from_utf8(msg.payload.to_vec()).expect("utf8 payload");
            acked.insert(payload_str);
            assert!(broker1.ack(sub1, tag));
        } else {
            break;
        }
    }

    // Simulate a crash by dropping the broker and WAL handles.
    simulate_crash(broker1);
    drop(wal);

    // Restart broker from WAL.
    let wal2 = Arc::new(
        WriteAheadLog::open(&path)
            .await
            .expect("failed to reopen WAL"),
    );
    let broker2 = Arc::new(Broker::new_with_wal(config, wal2.clone()));

    let client2 = ClientId::new("client-2");
    let sub2 = broker2.subscribe(client2, topic.clone(), QoSLevel::AtLeastOnce);

    // Replay all durable messages from WAL into the new broker.
    broker2
        .replay_from_wal()
        .await
        .expect("replay from WAL failed");

    // Collect all messages delivered after recovery.
    let mut seen = HashSet::new();
    loop {
        if let Some(msg) = broker2.poll(sub2) {
            let payload_str =
                String::from_utf8(msg.payload.to_vec()).expect("utf8 payload");
            seen.insert(payload_str);
            if let Some(tag) = msg.delivery_tag {
                let _ = broker2.ack(sub2, tag);
            }
        } else {
            break;
        }
    }

    // Verify that every originally published message is present after recovery.
    for payload in &payloads {
        assert!(
            seen.contains(payload),
            "missing payload after recovery: {}",
            payload
        );
    }

    // All unacked messages must have been redelivered; acked ones may also be
    // redelivered (at-least-once), but at minimum all unacked messages must be
    // observed.
    let unacked: Vec<_> = payloads
        .iter()
        .filter(|p| !acked.contains(*p))
        .collect();
    for payload in unacked {
        assert!(
            seen.contains(payload),
            "unacked payload not redelivered: {}",
            payload
        );
    }
}

