use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::sync::atomic::{AtomicBool, Ordering};

use blipmq::core::message::new_message;
use blipmq::core::queue::qos0::{OverflowPolicy, Queue};
use blipmq::core::queue::QueueBehavior;

#[test]
fn drop_new_rejects_incoming_message() {
    let q = Queue::new("q", 1, OverflowPolicy::DropNew);
    let m1 = Arc::new(new_message("m1"));
    let m2 = Arc::new(new_message("m2"));
    q.enqueue(m1.clone()).unwrap();
    assert!(q.enqueue(m2.clone()).is_err());
    assert_eq!(q.len(), 1);
    let stored = q.dequeue().unwrap();
    assert_eq!(stored.payload, b"m1");
}

#[test]
fn drop_oldest_replaces_existing_message() {
    let q = Queue::new("q", 1, OverflowPolicy::DropOldest);
    let m1 = Arc::new(new_message("m1"));
    let m2 = Arc::new(new_message("m2"));
    q.enqueue(m1.clone()).unwrap();
    q.enqueue(m2.clone()).unwrap();
    assert_eq!(q.len(), 1);
    let stored = q.dequeue().unwrap();
    assert_eq!(stored.payload, b"m2");
}

#[test]
fn block_waits_until_space_available() {
    let q = Arc::new(Queue::new("q", 1, OverflowPolicy::Block));
    let m1 = Arc::new(new_message("m1"));
    q.enqueue(m1).unwrap();

    let q_clone = q.clone();
    let m2 = Arc::new(new_message("m2"));
    let done = Arc::new(AtomicBool::new(false));
    let done_clone = done.clone();
    let handle = thread::spawn(move || {
        q_clone.enqueue(m2).unwrap();
        done_clone.store(true, Ordering::SeqCst);
    });

    thread::sleep(Duration::from_millis(50));
    assert!(!done.load(Ordering::SeqCst));
    let _ = q.dequeue();
    handle.join().unwrap();
    assert!(done.load(Ordering::SeqCst));
    let stored = q.dequeue().unwrap();
    assert_eq!(stored.payload, b"m2");
}