//! Performance optimization utilities and monitoring.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use once_cell::sync::Lazy;
use hdrhistogram::Histogram;
use std::sync::Mutex;

/// Performance metrics tracking
#[derive(Debug)]
pub struct PerformanceMetrics {
    // Connection metrics
    pub connections_accepted: AtomicU64,
    pub connections_dropped: AtomicU64,
    pub active_connections: AtomicU64,
    
    // Message metrics
    pub messages_published: AtomicU64,
    pub messages_delivered: AtomicU64,
    pub messages_dropped: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub bytes_received: AtomicU64,
    
    // Performance metrics
    pub publish_latency: Mutex<Histogram<u64>>,
    pub delivery_latency: Mutex<Histogram<u64>>,
    pub queue_depth_histogram: Mutex<Histogram<u64>>,
    
    // System metrics
    pub cpu_usage: AtomicU64, // in basis points (0-10000)
    pub memory_usage: AtomicU64, // in bytes
    pub gc_count: AtomicU64,
}

impl Default for PerformanceMetrics {
    fn default() -> Self {
        Self {
            connections_accepted: AtomicU64::new(0),
            connections_dropped: AtomicU64::new(0),
            active_connections: AtomicU64::new(0),
            messages_published: AtomicU64::new(0),
            messages_delivered: AtomicU64::new(0),
            messages_dropped: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            publish_latency: Mutex::new(Histogram::new(3).unwrap()),
            delivery_latency: Mutex::new(Histogram::new(3).unwrap()),
            queue_depth_histogram: Mutex::new(Histogram::new(2).unwrap()),
            cpu_usage: AtomicU64::new(0),
            memory_usage: AtomicU64::new(0),
            gc_count: AtomicU64::new(0),
        }
    }
}

impl PerformanceMetrics {
    pub fn record_publish_latency(&self, latency: Duration) {
        let micros = latency.as_micros() as u64;
        if let Ok(mut hist) = self.publish_latency.lock() {
            let _ = hist.record(micros);
        }
    }
    
    pub fn record_delivery_latency(&self, latency: Duration) {
        let micros = latency.as_micros() as u64;
        if let Ok(mut hist) = self.delivery_latency.lock() {
            let _ = hist.record(micros);
        }
    }
    
    pub fn record_queue_depth(&self, depth: usize) {
        if let Ok(mut hist) = self.queue_depth_histogram.lock() {
            let _ = hist.record(depth as u64);
        }
    }
    
    pub fn get_publish_latency_p99(&self) -> Option<Duration> {
        if let Ok(hist) = self.publish_latency.lock() {
            Some(Duration::from_micros(hist.value_at_quantile(0.99)))
        } else {
            None
        }
    }
    
    pub fn get_delivery_latency_p99(&self) -> Option<Duration> {
        if let Ok(hist) = self.delivery_latency.lock() {
            Some(Duration::from_micros(hist.value_at_quantile(0.99)))
        } else {
            None
        }
    }
    
    pub fn throughput_per_second(&self, window: Duration) -> f64 {
        let messages = self.messages_delivered.load(Ordering::Relaxed) as f64;
        messages / window.as_secs_f64()
    }
}

pub static PERFORMANCE_METRICS: Lazy<PerformanceMetrics> = Lazy::new(Default::default);

/// CPU core affinity utilities
pub mod affinity {
    use std::thread;
    
    pub fn pin_to_core(core_id: usize) -> Result<(), Box<dyn std::error::Error>> {
        // Platform-specific implementation would go here
        // For Linux: use core_affinity crate or libc calls
        // For now, we'll just log the intent
        tracing::info!("Would pin thread {:?} to core {}", thread::current().id(), core_id);
        Ok(())
    }
    
    pub fn get_cpu_count() -> usize {
        std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(1)
    }
    
    pub fn suggest_thread_pool_size() -> usize {
        // Use number of CPU cores, with minimum of 2 and maximum of 32
        get_cpu_count().clamp(2, 32)
    }
}

/// Memory optimization utilities
pub mod memory {
    use std::sync::atomic::{AtomicUsize, Ordering};
    
    static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
    
    pub fn track_allocation(size: usize) {
        ALLOCATED_BYTES.fetch_add(size, Ordering::Relaxed);
    }
    
    pub fn track_deallocation(size: usize) {
        ALLOCATED_BYTES.fetch_sub(size, Ordering::Relaxed);
    }
    
    pub fn get_allocated_bytes() -> usize {
        ALLOCATED_BYTES.load(Ordering::Relaxed)
    }
    
    /// Advice for memory optimization
    pub fn suggest_buffer_sizes() -> (usize, usize, usize) {
        let available_memory = get_available_memory();
        let small_buf = (available_memory / 10000).max(1024).min(4096);
        let medium_buf = (available_memory / 1000).max(4096).min(16384);
        let large_buf = (available_memory / 100).max(16384).min(65536);
        (small_buf, medium_buf, large_buf)
    }
    
    fn get_available_memory() -> usize {
        // Simplified - in real implementation, query system memory
        1024 * 1024 * 1024 // Assume 1GB available
    }
}

/// Network optimization utilities  
pub mod network {
    use tokio::net::TcpStream;
    use std::io::Result;
    
    pub fn optimize_tcp_socket(stream: &TcpStream) -> Result<()> {
        // Set TCP_NODELAY to disable Nagle's algorithm for low latency
        stream.set_nodelay(true)?;
        
        // Platform-specific socket optimizations would go here:
        // - TCP_USER_TIMEOUT
        // - SO_RCVBUF/SO_SNDBUF sizing  
        // - TCP_FASTOPEN
        // - SO_REUSEPORT for load balancing
        
        Ok(())
    }
    
    pub fn get_optimal_buffer_size() -> usize {
        // Calculate based on bandwidth-delay product
        // For now, use a reasonable default
        64 * 1024 // 64KB
    }
    
    pub fn get_suggested_backlog() -> u32 {
        // Suggest connection backlog size based on expected load
        1024
    }
}

/// Latency measurement utilities
pub struct LatencyTimer {
    start: Instant,
}

impl LatencyTimer {
    pub fn start() -> Self {
        Self {
            start: Instant::now(),
        }
    }
    
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
    
    pub fn record_publish_latency(self) {
        PERFORMANCE_METRICS.record_publish_latency(self.elapsed());
    }
    
    pub fn record_delivery_latency(self) {
        PERFORMANCE_METRICS.record_delivery_latency(self.elapsed());
    }
}

/// Performance monitoring task
pub async fn start_performance_monitor() {
    let mut interval = tokio::time::interval(Duration::from_secs(10));
    
    loop {
        interval.tick().await;
        
        let metrics = &*PERFORMANCE_METRICS;
        let active_connections = metrics.active_connections.load(Ordering::Relaxed);
        let messages_per_sec = metrics.throughput_per_second(Duration::from_secs(10));
        
        tracing::info!(
            "Performance: {} active connections, {:.2} msg/s, publish p99: {:?}, delivery p99: {:?}",
            active_connections,
            messages_per_sec,
            metrics.get_publish_latency_p99(),
            metrics.get_delivery_latency_p99()
        );
        
        // Record system metrics
        record_system_metrics().await;
    }
}

async fn record_system_metrics() {
    // In a real implementation, would gather:
    // - CPU usage via /proc/stat or similar
    // - Memory usage via /proc/meminfo
    // - Network interface statistics
    // - File descriptor count
    // - GC statistics (if applicable)
}