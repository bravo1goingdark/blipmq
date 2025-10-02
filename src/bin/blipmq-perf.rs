//! High-performance BlipMQ server

use std::net::{TcpListener, TcpStream};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
// use crossbeam::channel::{bounded, Sender, Receiver};
use dashmap::DashMap;

/// Server statistics
#[derive(Default)]
struct Stats {
    connections: AtomicU64,
    messages: AtomicU64,
    bytes: AtomicU64,
}

/// Connection handler
struct Connection {
    id: usize,
    stream: TcpStream,
    subscriptions: Vec<String>,
}

/// High-performance server
struct Server {
    stats: Arc<Stats>,
    shutdown: Arc<AtomicBool>,
    topics: Arc<DashMap<String, Vec<usize>>>,
    connections: Arc<DashMap<usize, std::sync::mpsc::Sender<Vec<u8>>>>,
}

impl Server {
    fn new() -> Self {
        Self {
            stats: Arc::new(Stats::default()),
            shutdown: Arc::new(AtomicBool::new(false)),
            topics: Arc::new(DashMap::new()),
            connections: Arc::new(DashMap::new()),
        }
    }
    
    fn run(&self, addr: &str) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind(addr)?;
        println!("🚀 High-performance server listening on {}", addr);
        
        let stats = Arc::clone(&self.stats);
        let shutdown = Arc::clone(&self.shutdown);
        
        // Stats thread
        let stats_thread = {
            let stats = Arc::clone(&stats);
            let shutdown = Arc::clone(&shutdown);
            
            thread::spawn(move || {
                let mut last_msgs = 0u64;
                let mut last_bytes = 0u64;
                
                while !shutdown.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_secs(5));
                    
                    let msgs = stats.messages.load(Ordering::Relaxed);
                    let bytes = stats.bytes.load(Ordering::Relaxed);
                    
                    let msg_rate = (msgs - last_msgs) / 5;
                    let byte_rate = (bytes - last_bytes) / 5;
                    
                    println!(
                        "📊 Stats - Connections: {}, Msg/s: {}, Bytes/s: {}",
                        stats.connections.load(Ordering::Relaxed),
                        msg_rate,
                        byte_rate
                    );
                    
                    last_msgs = msgs;
                    last_bytes = bytes;
                }
            })
        };
        
        // Accept connections
        let mut conn_id = 0usize;
        for stream in listener.incoming() {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }
            
            if let Ok(stream) = stream {
                conn_id += 1;
                self.stats.connections.fetch_add(1, Ordering::Relaxed);
                
                // Spawn connection handler
                let stats = Arc::clone(&self.stats);
                let topics = Arc::clone(&self.topics);
                let connections = Arc::clone(&self.connections);
                let shutdown = Arc::clone(&self.shutdown);
                
                thread::spawn(move || {
                    handle_connection(conn_id, stream, stats, topics, connections, shutdown);
                });
            }
        }
        
        stats_thread.join().ok();
        Ok(())
    }
    
    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

fn handle_connection(
    conn_id: usize,
    mut stream: TcpStream,
    stats: Arc<Stats>,
    topics: Arc<DashMap<String, Vec<usize>>>,
    connections: Arc<DashMap<usize, std::sync::mpsc::Sender<Vec<u8>>>>,
    shutdown: Arc<AtomicBool>,
) {
    println!("✅ Connection {} established", conn_id);
    
    // Create channel for outgoing messages
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    connections.insert(conn_id, tx);
    
    // Spawn writer thread
    let mut write_stream = stream.try_clone().unwrap();
    let writer = thread::spawn(move || {
        while let Ok(data) = rx.recv() {
            if write_stream.write_all(&data).is_err() {
                break;
            }
        }
    });
    
    // Read messages
    let mut buffer = [0u8; 65536];
    let mut subscriptions = Vec::new();
    
    while !shutdown.load(Ordering::Relaxed) {
        match stream.read(&mut buffer) {
            Ok(0) => break, // Connection closed
            Ok(n) => {
                stats.messages.fetch_add(1, Ordering::Relaxed);
                stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
                
                // Simple protocol parsing
                if n > 0 {
                    match buffer[0] {
                        b'P' => {
                            // Publish: P<topic_len><topic><payload>
                            if n > 2 {
                                let topic_len = buffer[1] as usize;
                                if n >= 2 + topic_len {
                                    let topic = String::from_utf8_lossy(&buffer[2..2+topic_len]).to_string();
                                    let payload = &buffer[2+topic_len..n];
                                    
                                    // Route to subscribers
                                    if let Some(subs) = topics.get(&topic) {
                                        for &sub_id in subs.value() {
                                            if sub_id != conn_id {
                                                if let Some(tx) = connections.get(&sub_id) {
                                                    tx.send(payload.to_vec()).ok();
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        b'S' => {
                            // Subscribe: S<topic_len><topic>
                            if n > 2 {
                                let topic_len = buffer[1] as usize;
                                if n >= 2 + topic_len {
                                    let topic = String::from_utf8_lossy(&buffer[2..2+topic_len]).to_string();
                                    
                                    topics.entry(topic.clone())
                                        .or_insert_with(Vec::new)
                                        .push(conn_id);
                                    
                                    subscriptions.push(topic);
                                    println!("📝 Connection {} subscribed", conn_id);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(_) => break,
        }
    }
    
    // Clean up
    connections.remove(&conn_id);
    for topic in subscriptions {
        if let Some(mut subs) = topics.get_mut(&topic) {
            subs.retain(|&id| id != conn_id);
        }
    }
    
    stats.connections.fetch_sub(1, Ordering::Relaxed);
    writer.join().ok();
    
    println!("❌ Connection {} closed", conn_id);
}

fn main() {
    println!("🚀 BlipMQ High-Performance Server");
    println!("==================================");
    
    let server = Server::new();
    
    // Handle Ctrl+C
    let shutdown = Arc::clone(&server.shutdown);
    ctrlc::set_handler(move || {
        println!("\n⚠️ Shutting down...");
        shutdown.store(true, Ordering::Relaxed);
    }).expect("Error setting Ctrl-C handler");
    
    // Run server
    if let Err(e) = server.run("0.0.0.0:5555") {
        eprintln!("Server error: {}", e);
    }
    
    println!("👋 Server stopped");
}
