//! High-performance message routing
//! 
//! Uses lock-free data structures and SPSC queues for each subscriber

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::collections::HashMap;
use crossbeam::channel::{Receiver, Sender, bounded};
use parking_lot::RwLock;
use bytes::Bytes;
use ahash::AHashMap;

pub use super::protocol::ParsedMessage;

/// Delivery task for delivery threads
#[derive(Debug)]
pub struct DeliveryTask {
    pub subscriber_id: usize,
    pub message: Arc<Message>,
}

/// Message to be delivered
#[derive(Debug)]
pub struct Message {
    pub topic: Bytes,
    pub payload: Bytes,
    pub timestamp: u64,
}

/// Subscriber information
struct Subscriber {
    id: usize,
    conn_id: usize,
    topics: Vec<String>,
    queue: Sender<Arc<Message>>,
}

/// High-performance router
pub struct Router {
    /// Topic -> Subscriber IDs mapping
    topics: RwLock<AHashMap<String, Vec<usize>>>,
    /// Subscriber ID -> Subscriber mapping
    subscribers: RwLock<AHashMap<usize, Arc<Subscriber>>>,
    /// Connection ID -> Subscriber IDs mapping
    connections: RwLock<AHashMap<usize, Vec<usize>>>,
    /// Next subscriber ID
    next_sub_id: AtomicUsize,
    /// Statistics
    pub messages_routed: AtomicUsize,
    pub messages_dropped: AtomicUsize,
}

impl Router {
    pub fn new() -> Self {
        Self {
            topics: RwLock::new(AHashMap::new()),
            subscribers: RwLock::new(AHashMap::new()),
            connections: RwLock::new(AHashMap::new()),
            next_sub_id: AtomicUsize::new(1),
            messages_routed: AtomicUsize::new(0),
            messages_dropped: AtomicUsize::new(0),
        }
    }
    
    /// Subscribe a connection to a topic
    fn subscribe(&self, conn_id: usize, topic: String) -> usize {
        let sub_id = self.next_sub_id.fetch_add(1, Ordering::SeqCst);
        
        // Create SPSC queue for this subscriber
        let (tx, rx) = bounded(10000);
        
        let subscriber = Arc::new(Subscriber {
            id: sub_id,
            conn_id,
            topics: vec![topic.clone()],
            queue: tx,
        });
        
        // Add to subscribers map
        self.subscribers.write().insert(sub_id, subscriber.clone());
        
        // Add to topic map
        self.topics.write()
            .entry(topic)
            .or_insert_with(Vec::new)
            .push(sub_id);
        
        // Add to connection map
        self.connections.write()
            .entry(conn_id)
            .or_insert_with(Vec::new)
            .push(sub_id);
        
        // TODO: Spawn delivery task for this subscriber
        // This would normally create a task that reads from rx and sends to conn_id
        
        sub_id
    }
    
    /// Unsubscribe a connection from a topic
    fn unsubscribe(&self, conn_id: usize, topic: &str) {
        // Find subscribers for this connection
        if let Some(sub_ids) = self.connections.read().get(&conn_id) {
            for &sub_id in sub_ids {
                // Remove from topic map
                if let Some(subs) = self.topics.write().get_mut(topic) {
                    subs.retain(|&id| id != sub_id);
                }
            }
        }
    }
    
    /// Route a message to all subscribers
    fn route_message(&self, topic: &str, message: Arc<Message>) -> usize {
        let mut delivered = 0;
        
        // Find all subscribers for this topic
        if let Some(sub_ids) = self.topics.read().get(topic) {
            for &sub_id in sub_ids {
                if let Some(subscriber) = self.subscribers.read().get(&sub_id) {
                    // Try to send to subscriber's queue (non-blocking)
                    match subscriber.queue.try_send(message.clone()) {
                        Ok(_) => {
                            delivered += 1;
                            self.messages_routed.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            // Queue full, drop message
                            self.messages_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
        
        delivered
    }
    
    /// Handle disconnect
    fn disconnect(&self, conn_id: usize) {
        // Remove all subscribers for this connection
        if let Some(sub_ids) = self.connections.write().remove(&conn_id) {
            for sub_id in sub_ids {
                // Remove from subscribers map
                if let Some(subscriber) = self.subscribers.write().remove(&sub_id) {
                    // Remove from all topics
                    for topic in &subscriber.topics {
                        if let Some(subs) = self.topics.write().get_mut(topic) {
                            subs.retain(|&id| id != sub_id);
                        }
                    }
                }
            }
        }
    }
}

/// Routing thread main loop
pub fn routing_thread_loop(
    thread_id: usize,
    rx: Receiver<ParsedMessage>,
    tx: Sender<DeliveryTask>,
    router: Arc<Router>,
    shutdown: Arc<AtomicBool>
) {
    println!("🚦 Routing thread {} starting", thread_id);
    
    let mut route_count = 0u64;
    
    while !shutdown.load(Ordering::Relaxed) {
        // Receive parsed message
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(msg) => {
                let topic_str = String::from_utf8_lossy(&msg.topic).to_string();
                
                match msg.op {
                    super::protocol::OpCode::Publish => {
                        // Route to all subscribers
                        let message = Arc::new(Message {
                            topic: msg.topic,
                            payload: msg.payload,
                            timestamp: msg.timestamp,
                        });
                        
                        let delivered = router.route_message(&topic_str, message);
                        route_count += delivered as u64;
                    }
                    super::protocol::OpCode::Subscribe => {
                        // Subscribe to topic
                        let sub_id = router.subscribe(msg.conn_id, topic_str);
                        println!("✅ Connection {} subscribed to topic (sub_id: {})", 
                                 msg.conn_id, sub_id);
                    }
                    super::protocol::OpCode::Unsubscribe => {
                        // Unsubscribe from topic
                        router.unsubscribe(msg.conn_id, &topic_str);
                        println!("❌ Connection {} unsubscribed from topic", msg.conn_id);
                    }
                    super::protocol::OpCode::Disconnect => {
                        // Handle disconnect
                        router.disconnect(msg.conn_id);
                        println!("🔌 Connection {} disconnected", msg.conn_id);
                    }
                    _ => {
                        // Other operations (ping, pong, etc.)
                    }
                }
            }
            Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                // No message, continue
            }
            Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
        
        // Log stats periodically
        if route_count % 100_000 == 0 && route_count > 0 {
            println!("Router {}: {} messages routed, {} dropped", 
                     thread_id, 
                     router.messages_routed.load(Ordering::Relaxed),
                     router.messages_dropped.load(Ordering::Relaxed));
        }
    }
    
    println!("🚦 Routing thread {} shutting down (routed: {})", thread_id, route_count);
}

// High-level routing engine for server
use dashmap::DashMap;

pub struct RoutingEngine {
    // Topic -> Set of subscriber connection IDs
    subscriptions: Arc<DashMap<String, Vec<usize>>>,
}

impl RoutingEngine {
    pub fn new() -> Self {
        Self {
            subscriptions: Arc::new(DashMap::new()),
        }
    }
    
    pub fn subscribe(&self, conn_id: usize, topic: String) {
        self.subscriptions
            .entry(topic)
            .or_insert_with(Vec::new)
            .push(conn_id);
    }
    
    pub fn unsubscribe(&self, conn_id: usize, topic: &str) {
        if let Some(mut subs) = self.subscriptions.get_mut(topic) {
            subs.retain(|&id| id != conn_id);
        }
    }
    
    pub fn get_subscribers(&self, topic: &str) -> Vec<usize> {
        self.subscriptions
            .get(topic)
            .map(|subs| subs.clone())
            .unwrap_or_default()
    }
    
    pub fn remove_connection(&self, conn_id: usize) {
        // Remove from all subscriptions
        for mut entry in self.subscriptions.iter_mut() {
            entry.value_mut().retain(|&id| id != conn_id);
        }
    }
}
