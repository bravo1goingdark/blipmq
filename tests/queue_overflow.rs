use std::sync::Arc;

use blipmq::core::message::{decode_frame, new_message, to_wire_message, ServerFrame};
use blipmq::core::queue::qos0::{OverflowPolicy, Queue};
use blipmq::core::queue::QueueBehavior;

#[test]
fn drop_new_rejects_incoming_message() {
    let q = Queue::new("q", 1, OverflowPolicy::DropNew);
    let m1 = Arc::new(to_wire_message(&new_message("m1")));
    let m2 = Arc::new(to_wire_message(&new_message("m2")));
    q.enqueue(m1.clone()).unwrap();
    assert!(q.enqueue(m2.clone()).is_err());
    assert_eq!(q.len(), 1);
    let stored = q.dequeue().unwrap();
    let buf = &stored.frame;
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let payload = &buf[4..4 + len];
    match decode_frame(payload).unwrap() {
        ServerFrame::Message(m) => assert_eq!(m.payload.as_ref(), b"m1"),
        _ => panic!("unexpected frame"),
    }
}

#[test]
fn drop_oldest_replaces_existing_message() {
    let q = Queue::new("q", 1, OverflowPolicy::DropOldest);
    let m1 = Arc::new(to_wire_message(&new_message("m1")));
    let m2 = Arc::new(to_wire_message(&new_message("m2")));
    q.enqueue(m1.clone()).unwrap();
    q.enqueue(m2.clone()).unwrap();
    assert_eq!(q.len(), 1);
    let stored = q.dequeue().unwrap();
    let buf = &stored.frame;
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let payload = &buf[4..4 + len];
    match decode_frame(payload).unwrap() {
        ServerFrame::Message(m) => assert_eq!(m.payload.as_ref(), b"m2"),
        _ => panic!("unexpected frame"),
    }
}
