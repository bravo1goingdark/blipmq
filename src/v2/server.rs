//! High-performance server with platform-specific I/O

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::net::SocketAddr;
use std::thread;
use std::time::{Duration, Instant};
use bytes::{Bytes, BytesMut};
use crossbeam::channel::{Sender, Receiver, bounded};
use parking_lot::RwLock;
use dashmap::DashMap;
use tracing::{info, warn, error, debug};

#[cfg(target_os = "windows")]
use crate::v2::iocp_impl::{IocpBackend, IoEvent, EventType};

use crate::v2::protocol::{ProtocolParser, ParsedMessage, MessageType};
use crate::v2::routing::RoutingEngine;

/// Server statistics
#[derive(Debug, Default)]
pub struct ServerStats {
    pub total_connections: AtomicU64,
    pub active_connections: AtomicU64,
    pub messages_received: AtomicU64,
    pub messages_sent: AtomicU64,
    pub bytes_received: AtomicU64,
    pub bytes_sent: AtomicU64,
}

/// Connection state
struct ConnectionState {
    id: usize,
    addr: SocketAddr,
    subscriptions: Vec<String>,
    parser: ProtocolParser,
    last_activity: Instant,
}

/// High-performance server
pub struct HighPerfServer {
    addr: SocketAddr,
    stats: Arc<ServerStats>,
    connections: Arc<DashMap<usize, ConnectionState>>,
    routing_engine: Arc<RoutingEngine>,
    shutdown: Arc<AtomicBool>,
    num_io_threads: usize,
    num_worker_threads: usize,
}

impl HighPerfServer {
    pub fn new(addr: SocketAddr) -> Self {
        let num_cpus = num_cpus::get();
        
        Self {
            addr,
            stats: Arc::new(ServerStats::default()),
            connections: Arc::new(DashMap::with_capacity(10000)),
            routing_engine: Arc::new(RoutingEngine::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            num_io_threads: num_cpus / 2,  // Half for I/O
            num_worker_threads: num_cpus / 2,  // Half for processing
        }
    }
    
    /// Start the server
    pub fn start(&self) -> Result<(), Box<dyn std::error::Error>> {
        info!("Starting high-performance server on {}", self.addr);
        
        // Create message channels
        let (msg_tx, msg_rx) = bounded::<ProcessMessage>(65536);
        let (out_tx, out_rx) = bounded::<OutgoingMessage>(65536);
        
        // Start I/O thread
        let io_handle = self.start_io_thread(msg_tx.clone(), out_rx)?;
        
        // Start worker threads
        let worker_handles = self.start_worker_threads(msg_rx, out_tx.clone());
        
        // Start monitoring thread
        self.start_monitoring_thread();
        
        // Wait for shutdown
        while !self.shutdown.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_secs(1));
        }
        
        info!("Server shutting down...");
        
        // Clean shutdown
        io_handle.join().ok();
        for handle in worker_handles {
            handle.join().ok();
        }
        
        Ok(())
    }
    
    /// Start I/O thread with IOCP
    #[cfg(target_os = "windows")]
    fn start_io_thread(
        &self,
        msg_tx: Sender<ProcessMessage>,
        out_rx: Receiver<OutgoingMessage>,
    ) -> Result<thread::JoinHandle<()>, Box<dyn std::error::Error>> {
        let addr = self.addr;
        let connections = Arc::clone(&self.connections);
        let stats = Arc::clone(&self.stats);
        let shutdown = Arc::clone(&self.shutdown);
        
        let handle = thread::spawn(move || {
            let mut backend = match IocpBackend::new(2) {
                Ok(b) => b,
                Err(e) => {
                    error!("Failed to create IOCP backend: {}", e);
                    return;
                }
            };
            
            if let Err(e) = backend.init(addr) {
                error!("Failed to initialize IOCP: {}", e);
                return;
            }
            
            info!("IOCP initialized, listening on {}", addr);
            
            // Event loop
            while !shutdown.load(Ordering::Relaxed) {
                // Poll for I/O events
                match backend.poll(10) {
                    Ok(events) => {
                        for event in events {
                            match event.event_type {
                                EventType::Accept => {
                                    let conn_state = ConnectionState {
                                        id: event.conn_id,
                                        addr: "0.0.0.0:0".parse().unwrap(), // TODO: Get real addr
                                        subscriptions: Vec::new(),
                                        parser: ProtocolParser::new(),
                                        last_activity: Instant::now(),
                                    };
                                    
                                    connections.insert(event.conn_id, conn_state);
                                    stats.active_connections.fetch_add(1, Ordering::Relaxed);
                                    stats.total_connections.fetch_add(1, Ordering::Relaxed);
                                    
                                    debug!("New connection: {}", event.conn_id);
                                }
                                EventType::Read => {
                                    if let Some(data) = event.data {
                                        stats.bytes_received.fetch_add(data.len() as u64, Ordering::Relaxed);
                                        
                                        // Parse and process message
                                        if let Some(mut conn) = connections.get_mut(&event.conn_id) {
                                            conn.last_activity = Instant::now();
                                            
                                            if let Ok(messages) = conn.parser.parse(&data) {
                                                for msg in messages {
                                                    stats.messages_received.fetch_add(1, Ordering::Relaxed);
                                                    
                                                    let process_msg = ProcessMessage {
                                                        conn_id: event.conn_id,
                                                        message: msg,
                                                    };
                                                    
                                                    if msg_tx.try_send(process_msg).is_err() {
                                                        warn!("Message queue full");
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                EventType::Write => {
                                    // Write completed
                                }
                                EventType::Close => {
                                    connections.remove(&event.conn_id);
                                    stats.active_connections.fetch_sub(1, Ordering::Relaxed);
                                    debug!("Connection closed: {}", event.conn_id);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("IOCP poll error: {}", e);
                    }
                }
                
                // Process outgoing messages
                while let Ok(out_msg) = out_rx.try_recv() {
                    if let Err(e) = backend.post_write(out_msg.conn_id, out_msg.data.clone()) {
                        warn!("Failed to send message: {}", e);
                    } else {
                        stats.messages_sent.fetch_add(1, Ordering::Relaxed);
                        stats.bytes_sent.fetch_add(out_msg.data.len() as u64, Ordering::Relaxed);
                    }
                }
            }
            
            info!("I/O thread shutting down");
        });
        
        Ok(handle)
    }
    
    /// Start worker threads for message processing
    fn start_worker_threads(
        &self,
        msg_rx: Receiver<ProcessMessage>,
        out_tx: Sender<OutgoingMessage>,
    ) -> Vec<thread::JoinHandle<()>> {
        let mut handles = Vec::new();
        
        for i in 0..self.num_worker_threads {
            let msg_rx = msg_rx.clone();
            let out_tx = out_tx.clone();
            let connections = Arc::clone(&self.connections);
            let routing = Arc::clone(&self.routing_engine);
            let shutdown = Arc::clone(&self.shutdown);
            
            let handle = thread::spawn(move || {
                debug!("Worker thread {} started", i);
                
                while !shutdown.load(Ordering::Relaxed) {
                    match msg_rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(msg) => {
                            // Process message based on type
                            match msg.message.msg_type() {
                                MessageType::Publish => {
                                    // Route to subscribers
                                    let topic = String::from_utf8_lossy(&msg.message.topic).to_string();
                                    if !topic.is_empty() {
                                        let subscribers = routing.get_subscribers(&topic);
                                        
                                        for sub_id in subscribers {
                                            if sub_id != msg.conn_id {
                                                // Forward message
                                                let out_msg = OutgoingMessage {
                                                    conn_id: sub_id,
                                                    data: msg.message.payload.clone(),
                                                };
                                                
                                                if out_tx.try_send(out_msg).is_err() {
                                                    warn!("Output queue full");
                                                }
                                            }
                                        }
                                    }
                                }
                                MessageType::Subscribe => {
                                    let topic = String::from_utf8_lossy(&msg.message.topic).to_string();
                                    if !topic.is_empty() {
                                        routing.subscribe(msg.conn_id, topic.clone());
                                        
                                        if let Some(mut conn) = connections.get_mut(&msg.conn_id) {
                                            conn.subscriptions.push(topic);
                                        }
                                    }
                                }
                                MessageType::Unsubscribe => {
                                    let topic = String::from_utf8_lossy(&msg.message.topic).to_string();
                                    if !topic.is_empty() {
                                        routing.unsubscribe(msg.conn_id, &topic);
                                        
                                        if let Some(mut conn) = connections.get_mut(&msg.conn_id) {
                                            conn.subscriptions.retain(|t| t != &topic);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        Err(_) => {
                            // Timeout, check shutdown
                        }
                    }
                }
                
                debug!("Worker thread {} shutting down", i);
            });
            
            handles.push(handle);
        }
        
        handles
    }
    
    /// Start monitoring thread
    fn start_monitoring_thread(&self) {
        let stats = Arc::clone(&self.stats);
        let shutdown = Arc::clone(&self.shutdown);
        
        thread::spawn(move || {
            let mut last_msgs_recv = 0u64;
            let mut last_msgs_sent = 0u64;
            let mut last_bytes_recv = 0u64;
            let mut last_bytes_sent = 0u64;
            
            while !shutdown.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_secs(5));
                
                let msgs_recv = stats.messages_received.load(Ordering::Relaxed);
                let msgs_sent = stats.messages_sent.load(Ordering::Relaxed);
                let bytes_recv = stats.bytes_received.load(Ordering::Relaxed);
                let bytes_sent = stats.bytes_sent.load(Ordering::Relaxed);
                
                let msg_recv_rate = (msgs_recv - last_msgs_recv) / 5;
                let msg_sent_rate = (msgs_sent - last_msgs_sent) / 5;
                let bytes_recv_rate = (bytes_recv - last_bytes_recv) / 5;
                let bytes_sent_rate = (bytes_sent - last_bytes_sent) / 5;
                
                info!(
                    "Stats - Connections: {}, Msg/s: R:{} S:{}, Bytes/s: R:{} S:{}",
                    stats.active_connections.load(Ordering::Relaxed),
                    msg_recv_rate,
                    msg_sent_rate,
                    bytes_recv_rate,
                    bytes_sent_rate,
                );
                
                last_msgs_recv = msgs_recv;
                last_msgs_sent = msgs_sent;
                last_bytes_recv = bytes_recv;
                last_bytes_sent = bytes_sent;
            }
        });
    }
    
    /// Shutdown the server
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

/// Message to process
struct ProcessMessage {
    conn_id: usize,
    message: ParsedMessage,
}

/// Outgoing message
struct OutgoingMessage {
    conn_id: usize,
    data: Bytes,
}

#[cfg(not(target_os = "windows"))]
fn start_io_thread(
    &self,
    msg_tx: Sender<ProcessMessage>,
    out_rx: Receiver<OutgoingMessage>,
) -> Result<thread::JoinHandle<()>, Box<dyn std::error::Error>> {
    // Fallback implementation for non-Windows platforms
    todo!("Implement io_uring for Linux or mio fallback")
}
