use std::sync::Arc;

use blipmq::core::message::to_wire_message;
use blipmq::core::queue::qos0::{OverflowPolicy, Queue};
use blipmq::core::queue::QueueBehavior;

#[test]
fn overflow_volume_drop_oldest_keeps_newest() {
    let cap = 8usize;
    let q = Queue::new("q", cap, OverflowPolicy::DropOldest);

    // Enqueue more than capacity
    for i in 0..100u64 {
        let payload = bytes::Bytes::from(format!("m{i}"));
        let msg = blipmq::core::message::with_custom_message(i, payload, 0, 0);
        let w = Arc::new(to_wire_message(&msg));
        let _ = q.enqueue(w);
    }

    // Drain everything
    let drained = q.drain(1024);
    assert_eq!(drained.len(), cap);

    // Expect last `cap` messages retained: 92..=99
    let mut ids: Vec<u64> = Vec::new();
    for wm in drained {
        // Parse the length-prefixed frame and extract payload ID back
        let buf = wm.frame.clone();
        let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        // Skip the type byte after length prefix
        let payload = &buf[5..4 + len];
        let m = blipmq::core::message::decode_message(payload).unwrap();
        ids.push(m.id);
    }
    assert_eq!(ids, (100 - cap as u64..100).collect::<Vec<_>>());
}
