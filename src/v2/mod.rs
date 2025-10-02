//! BlipMQ v2 - Production-grade high-performance message broker
//! 
//! Architecture:
//! - Stage 1: Network I/O threads (accept connections, read/write)
//! - Stage 2: Protocol parsing threads (zero-copy parsing)
//! - Stage 3: Routing threads (topic matching, fanout)
//! - Stage 4: Delivery threads (subscriber queues)
//! 
//! All stages connected via SPSC queues for maximum performance

pub mod network;
pub mod protocol;
pub mod delivery;
pub mod routing;
pub mod ultra_server;
pub mod simple_ultra_server;
// pub mod fast_io;  // Temporarily disabled - has compilation issues
// pub mod server;   // Temporarily disabled - depends on fast_io

// #[cfg(target_os = "windows")]
// pub mod iocp_impl;  // Temporarily disabled - has compilation issues
pub mod metrics;

use std::sync::Arc;
use std::thread;
use crossbeam::channel::{bounded, Sender, Receiver};

/// High-performance broker configuration
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    /// Number of I/O threads
    pub io_threads: usize,
    /// Number of parser threads
    pub parser_threads: usize,
    /// Number of routing threads
    pub routing_threads: usize,
    /// Number of delivery threads
    pub delivery_threads: usize,
    /// TCP listen address
    pub listen_addr: String,
    /// Max connections
    pub max_connections: usize,
    /// Connection buffer size
    pub buffer_size: usize,
    /// Queue depth between stages
    pub queue_depth: usize,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        let num_cpus = num_cpus::get();
        Self {
            io_threads: (num_cpus / 4).max(2),
            parser_threads: (num_cpus / 4).max(2),
            routing_threads: (num_cpus / 4).max(2),
            delivery_threads: (num_cpus / 4).max(2),
            listen_addr: "0.0.0.0:9999".to_string(),
            max_connections: 100_000,
            buffer_size: 65536,
            queue_depth: 100_000,
        }
    }
}

/// Main broker structure
pub struct Broker {
    config: BrokerConfig,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl Broker {
    pub fn new(config: BrokerConfig) -> Self {
        Self {
            config,
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
    
    /// Start the broker with all processing stages
    pub fn start(self) -> Result<(), Box<dyn std::error::Error>> {
        println!("🚀 Starting BlipMQ v2 High-Performance Broker");
        println!("   I/O threads: {}", self.config.io_threads);
        println!("   Parser threads: {}", self.config.parser_threads);
        println!("   Routing threads: {}", self.config.routing_threads);
        println!("   Delivery threads: {}", self.config.delivery_threads);
        
        // Create channels between stages (SPSC for each pair)
        let (io_to_parser_tx, io_to_parser_rx) = bounded(self.config.queue_depth);
        let (parser_to_routing_tx, parser_to_routing_rx) = bounded(self.config.queue_depth);
        let (routing_to_delivery_tx, routing_to_delivery_rx) = bounded(self.config.queue_depth);
        
        // Start delivery threads (Stage 4)
        let delivery_handles = self.start_delivery_threads(routing_to_delivery_rx);
        
        // Start routing threads (Stage 3)
        let routing_handles = self.start_routing_threads(
            parser_to_routing_rx,
            routing_to_delivery_tx
        );
        
        // Start parser threads (Stage 2)
        let parser_handles = self.start_parser_threads(
            io_to_parser_rx,
            parser_to_routing_tx
        );
        
        // Start I/O threads (Stage 1)
        let io_handles = self.start_io_threads(io_to_parser_tx)?;
        
        // Wait for shutdown signal
        self.wait_for_shutdown();
        
        // Clean shutdown
        println!("Shutting down broker...");
        self.shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        
        // Wait for all threads
        for handle in io_handles {
            handle.join().expect("IO thread panicked");
        }
        for handle in parser_handles {
            handle.join().expect("Parser thread panicked");
        }
        for handle in routing_handles {
            handle.join().expect("Routing thread panicked");
        }
        for handle in delivery_handles {
            handle.join().expect("Delivery thread panicked");
        }
        
        println!("✅ Broker shutdown complete");
        Ok(())
    }
    
    fn start_io_threads(&self, tx: Sender<network::RawMessage>) 
        -> Result<Vec<thread::JoinHandle<()>>, Box<dyn std::error::Error>> {
        let mut handles = Vec::new();
        
        for i in 0..self.config.io_threads {
            let config = self.config.clone();
            let tx = tx.clone();
            let shutdown = self.shutdown.clone();
            
            let handle = thread::Builder::new()
                .name(format!("io-{}", i))
                .spawn(move || {
                    // Pin to CPU core for better cache locality
                    #[cfg(target_os = "linux")]
                    {
                        use core_affinity::CoreId;
                        core_affinity::set_for_current(CoreId { id: i });
                    }
                    
                    network::io_thread_loop(i, config, tx, shutdown);
                })?;
            
            handles.push(handle);
        }
        
        Ok(handles)
    }
    
    fn start_parser_threads(&self, rx: Receiver<network::RawMessage>, 
                           tx: Sender<routing::ParsedMessage>) 
        -> Vec<thread::JoinHandle<()>> {
        let mut handles = Vec::new();
        
        for i in 0..self.config.parser_threads {
            let rx = rx.clone();
            let tx = tx.clone();
            let shutdown = self.shutdown.clone();
            
            let handle = thread::Builder::new()
                .name(format!("parser-{}", i))
                .spawn(move || {
                    protocol::parser_thread_loop(i, rx, tx, shutdown);
                })
                .expect("Failed to spawn parser thread");
            
            handles.push(handle);
        }
        
        handles
    }
    
    fn start_routing_threads(&self, rx: Receiver<routing::ParsedMessage>,
                            tx: Sender<delivery::DeliveryTask>) 
        -> Vec<thread::JoinHandle<()>> {
        let mut handles = Vec::new();
        let router = Arc::new(routing::Router::new());
        
        for i in 0..self.config.routing_threads {
            let rx = rx.clone();
            let tx = tx.clone();
            let router = router.clone();
            let shutdown = self.shutdown.clone();
            
            let handle = thread::Builder::new()
                .name(format!("router-{}", i))
                .spawn(move || {
                    routing::routing_thread_loop(i, rx, tx, router, shutdown);
                })
                .expect("Failed to spawn routing thread");
            
            handles.push(handle);
        }
        
        handles
    }
    
    fn start_delivery_threads(&self, rx: Receiver<delivery::DeliveryTask>) 
        -> Vec<thread::JoinHandle<()>> {
        let mut handles = Vec::new();
        
        for i in 0..self.config.delivery_threads {
            let rx = rx.clone();
            let shutdown = self.shutdown.clone();
            
            let handle = thread::Builder::new()
                .name(format!("delivery-{}", i))
                .spawn(move || {
                    delivery::delivery_thread_loop(i, rx, shutdown);
                })
                .expect("Failed to spawn delivery thread");
            
            handles.push(handle);
        }
        
        handles
    }
    
    fn wait_for_shutdown(&self) {
        // Wait for Ctrl+C
        let shutdown = self.shutdown.clone();
        ctrlc::set_handler(move || {
            println!("\n📛 Received shutdown signal");
            shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        }).expect("Error setting Ctrl-C handler");
        
        // Block until shutdown
        while !self.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}
