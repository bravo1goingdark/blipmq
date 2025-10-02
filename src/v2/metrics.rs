//! Performance metrics collection

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub struct Metrics {
    pub messages_received: AtomicU64,
    pub messages_parsed: AtomicU64,
    pub messages_routed: AtomicU64,
    pub messages_delivered: AtomicU64,
    pub messages_dropped: AtomicU64,
    pub bytes_received: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub connections_accepted: AtomicU64,
    pub connections_closed: AtomicU64,
    pub parse_errors: AtomicU64,
    start_time: Instant,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            messages_received: AtomicU64::new(0),
            messages_parsed: AtomicU64::new(0),
            messages_routed: AtomicU64::new(0),
            messages_delivered: AtomicU64::new(0),
            messages_dropped: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            connections_accepted: AtomicU64::new(0),
            connections_closed: AtomicU64::new(0),
            parse_errors: AtomicU64::new(0),
            start_time: Instant::now(),
        }
    }
    
    pub fn uptime(&self) -> Duration {
        self.start_time.elapsed()
    }
    
    pub fn throughput(&self) -> f64 {
        let messages = self.messages_delivered.load(Ordering::Relaxed) as f64;
        let seconds = self.uptime().as_secs_f64();
        if seconds > 0.0 {
            messages / seconds
        } else {
            0.0
        }
    }
    
    pub fn report(&self) {
        println!("\n📊 Performance Metrics");
        println!("======================");
        println!("Uptime: {:?}", self.uptime());
        println!("Messages:");
        println!("  Received: {}", self.messages_received.load(Ordering::Relaxed));
        println!("  Parsed: {}", self.messages_parsed.load(Ordering::Relaxed));
        println!("  Routed: {}", self.messages_routed.load(Ordering::Relaxed));
        println!("  Delivered: {}", self.messages_delivered.load(Ordering::Relaxed));
        println!("  Dropped: {}", self.messages_dropped.load(Ordering::Relaxed));
        println!("Network:");
        println!("  Bytes received: {}", self.bytes_received.load(Ordering::Relaxed));
        println!("  Bytes sent: {}", self.bytes_sent.load(Ordering::Relaxed));
        println!("  Connections accepted: {}", self.connections_accepted.load(Ordering::Relaxed));
        println!("  Connections closed: {}", self.connections_closed.load(Ordering::Relaxed));
        println!("Performance:");
        println!("  Throughput: {:.0} msg/s", self.throughput());
        println!("  Parse errors: {}", self.parse_errors.load(Ordering::Relaxed));
    }
}
