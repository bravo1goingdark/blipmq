//! Advanced connection management with rate limiting, backpressure, and resilience.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Semaphore};
use tokio::time::sleep;
use dashmap::DashMap;

/// Connection limits and rate limiting configuration
#[derive(Debug, Clone)]
pub struct ConnectionLimits {
    pub max_connections: usize,
    pub max_connections_per_ip: usize,
    pub connection_timeout: Duration,
    pub idle_timeout: Duration,
    pub rate_limit_per_connection: u32, // messages per second
    pub rate_limit_window: Duration,
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            max_connections: 10000,
            max_connections_per_ip: 100,
            connection_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(300), // 5 minutes
            rate_limit_per_connection: 1000, // 1000 msg/s per connection
            rate_limit_window: Duration::from_secs(1),
        }
    }
}

/// Per-connection state and rate limiting
#[derive(Debug)]
pub struct ConnectionState {
    pub addr: SocketAddr,
    pub connected_at: Instant,
    pub last_activity: AtomicU64, // timestamp in millis
    pub message_count: AtomicU32,
    pub bytes_sent: AtomicU64,
    pub bytes_received: AtomicU64,
    pub rate_limiter: TokenBucket,
}

impl ConnectionState {
    pub fn new(addr: SocketAddr, rate_limit: u32) -> Self {
        let now = Instant::now();
        Self {
            addr,
            connected_at: now,
            last_activity: AtomicU64::new(now.elapsed().as_millis() as u64),
            message_count: AtomicU32::new(0),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            rate_limiter: TokenBucket::new(rate_limit, rate_limit),
        }
    }
    
    pub fn update_activity(&self) {
        self.last_activity.store(
            Instant::now().elapsed().as_millis() as u64,
            Ordering::Relaxed
        );
    }
    
    pub fn check_rate_limit(&self) -> bool {
        self.rate_limiter.try_consume(1)
    }
    
    pub fn is_idle(&self, timeout: Duration) -> bool {
        let last_activity = Duration::from_millis(self.last_activity.load(Ordering::Relaxed));
        Instant::now().duration_since(self.connected_at) - last_activity > timeout
    }
}

/// Token bucket for rate limiting
#[derive(Debug)]
pub struct TokenBucket {
    tokens: AtomicU32,
    capacity: u32,
    refill_rate: u32,
    last_refill: AtomicU64, // timestamp in millis
}

impl TokenBucket {
    pub fn new(capacity: u32, refill_rate: u32) -> Self {
        Self {
            tokens: AtomicU32::new(capacity),
            capacity,
            refill_rate,
            last_refill: AtomicU64::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64
            ),
        }
    }
    
    pub fn try_consume(&self, tokens: u32) -> bool {
        self.refill();
        
        loop {
            let current = self.tokens.load(Ordering::Relaxed);
            if current < tokens {
                return false;
            }
            
            if self.tokens.compare_exchange_weak(
                current,
                current - tokens,
                Ordering::Relaxed,
                Ordering::Relaxed
            ).is_ok() {
                return true;
            }
        }
    }
    
    fn refill(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        
        let last_refill = self.last_refill.load(Ordering::Relaxed);
        let elapsed_ms = now.saturating_sub(last_refill);
        
        if elapsed_ms >= 1000 { // Refill every second
            let tokens_to_add = (elapsed_ms / 1000) as u32 * self.refill_rate;
            if tokens_to_add > 0 {
                loop {
                    let current = self.tokens.load(Ordering::Relaxed);
                    let new_tokens = (current + tokens_to_add).min(self.capacity);
                    
                    if self.tokens.compare_exchange_weak(
                        current,
                        new_tokens,
                        Ordering::Relaxed,
                        Ordering::Relaxed
                    ).is_ok() {
                        self.last_refill.store(now, Ordering::Relaxed);
                        break;
                    }
                }
            }
        }
    }
}

/// Connection manager with advanced features
#[derive(Debug)]
pub struct ConnectionManager {
    limits: ConnectionLimits,
    connections: DashMap<String, Arc<ConnectionState>>,
    ip_connection_counts: DashMap<std::net::IpAddr, AtomicU32>,
    connection_semaphore: Arc<Semaphore>,
    total_connections: AtomicU32,
}

impl ConnectionManager {
    pub fn new(limits: ConnectionLimits) -> Self {
        Self {
            connection_semaphore: Arc::new(Semaphore::new(limits.max_connections)),
            limits,
            connections: DashMap::new(),
            ip_connection_counts: DashMap::new(),
            total_connections: AtomicU32::new(0),
        }
    }
    
    /// Try to accept a new connection with rate limiting and IP-based limits
    pub async fn try_accept_connection(&self, addr: SocketAddr) -> Result<Arc<ConnectionState>, ConnectionError> {
        // Check global connection limit
        let _permit = self.connection_semaphore
            .try_acquire()
            .map_err(|_| ConnectionError::TooManyConnections)?;
        
        // Check per-IP connection limit
        let ip_count = self.ip_connection_counts
            .entry(addr.ip())
            .or_insert_with(|| AtomicU32::new(0));
        
        let current_ip_connections = ip_count.fetch_add(1, Ordering::Relaxed);
        if current_ip_connections >= self.limits.max_connections_per_ip as u32 {
            ip_count.fetch_sub(1, Ordering::Relaxed);
            return Err(ConnectionError::TooManyConnectionsFromIP);
        }
        
        // Create connection state
        let connection_id = format!("{}:{}", addr.ip(), addr.port());
        let state = Arc::new(ConnectionState::new(addr, self.limits.rate_limit_per_connection));
        
        self.connections.insert(connection_id, state.clone());
        self.total_connections.fetch_add(1, Ordering::Relaxed);
        
        // Update metrics
        crate::core::performance::PERFORMANCE_METRICS
            .connections_accepted.fetch_add(1, Ordering::Relaxed);
        crate::core::performance::PERFORMANCE_METRICS
            .active_connections.store(self.total_connections.load(Ordering::Relaxed) as u64, Ordering::Relaxed);
        
        // Don't drop the permit - it will be held until connection is closed
        std::mem::forget(_permit);
        
        Ok(state)
    }
    
    /// Remove a connection and cleanup resources
    pub fn remove_connection(&self, addr: SocketAddr) {
        let connection_id = format!("{}:{}", addr.ip(), addr.port());
        
        if self.connections.remove(&connection_id).is_some() {
            // Decrement IP counter
            if let Some(ip_count) = self.ip_connection_counts.get(&addr.ip()) {
                ip_count.fetch_sub(1, Ordering::Relaxed);
                
                // Remove IP entry if no more connections
                if ip_count.load(Ordering::Relaxed) == 0 {
                    self.ip_connection_counts.remove(&addr.ip());
                }
            }
            
            self.total_connections.fetch_sub(1, Ordering::Relaxed);
            self.connection_semaphore.add_permits(1);
            
            // Update metrics
            crate::core::performance::PERFORMANCE_METRICS
                .connections_dropped.fetch_add(1, Ordering::Relaxed);
            crate::core::performance::PERFORMANCE_METRICS
                .active_connections.store(self.total_connections.load(Ordering::Relaxed) as u64, Ordering::Relaxed);
        }
    }
    
    /// Check if a connection can send messages (rate limiting)
    pub fn check_rate_limit(&self, addr: SocketAddr) -> bool {
        let connection_id = format!("{}:{}", addr.ip(), addr.port());
        
        if let Some(state) = self.connections.get(&connection_id) {
            state.check_rate_limit()
        } else {
            false // Connection not found
        }
    }
    
    /// Update connection activity
    pub fn update_activity(&self, addr: SocketAddr) {
        let connection_id = format!("{}:{}", addr.ip(), addr.port());
        
        if let Some(state) = self.connections.get(&connection_id) {
            state.update_activity();
        }
    }
    
    /// Get connection statistics
    pub fn get_connection_stats(&self) -> ConnectionStats {
        ConnectionStats {
            total_connections: self.total_connections.load(Ordering::Relaxed),
            connections_by_ip: self.ip_connection_counts.len() as u32,
            available_permits: self.connection_semaphore.available_permits() as u32,
        }
    }
    
    /// Background task to cleanup idle connections
    pub async fn cleanup_idle_connections(&self) {
        let mut interval = tokio::time::interval(Duration::from_secs(60)); // Check every minute
        
        loop {
            interval.tick().await;
            
            let idle_timeout = self.limits.idle_timeout;
            let mut to_remove = Vec::new();
            
            // Find idle connections
            for entry in self.connections.iter() {
                let connection_id = entry.key();
                let state = entry.value();
                
                if state.is_idle(idle_timeout) {
                    to_remove.push((state.addr, connection_id.clone()));
                }
            }
            
            // Remove idle connections
            for (addr, connection_id) in to_remove {
                if self.connections.remove(&connection_id).is_some() {
                    tracing::info!("Removed idle connection: {}", addr);
                    self.remove_connection(addr);
                }
            }
        }
    }
}

/// Connection errors
#[derive(Debug, Clone)]
pub enum ConnectionError {
    TooManyConnections,
    TooManyConnectionsFromIP,
    RateLimitExceeded,
    ConnectionTimeout,
}

impl std::fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionError::TooManyConnections => write!(f, "Too many total connections"),
            ConnectionError::TooManyConnectionsFromIP => write!(f, "Too many connections from this IP"),
            ConnectionError::RateLimitExceeded => write!(f, "Rate limit exceeded"),
            ConnectionError::ConnectionTimeout => write!(f, "Connection timeout"),
        }
    }
}

impl std::error::Error for ConnectionError {}

/// Connection statistics
#[derive(Debug, Clone)]
pub struct ConnectionStats {
    pub total_connections: u32,
    pub connections_by_ip: u32,
    pub available_permits: u32,
}

/// Circuit breaker for handling downstream failures
#[derive(Debug)]
pub struct CircuitBreaker {
    failure_count: AtomicU32,
    success_count: AtomicU32,
    last_failure: AtomicU64,
    state: AtomicU8, // 0 = Closed, 1 = Open, 2 = HalfOpen
    failure_threshold: u32,
    success_threshold: u32,
    timeout: Duration,
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, success_threshold: u32, timeout: Duration) -> Self {
        Self {
            failure_count: AtomicU32::new(0),
            success_count: AtomicU32::new(0),
            last_failure: AtomicU64::new(0),
            state: AtomicU8::new(0), // Closed
            failure_threshold,
            success_threshold,
            timeout,
        }
    }
    
    pub fn call<F, T, E>(&self, f: F) -> Result<T, CircuitBreakerError<E>>
    where
        F: FnOnce() -> Result<T, E>,
    {
        match self.get_state() {
            CircuitState::Open => {
                if self.should_try() {
                    self.set_state(CircuitState::HalfOpen);
                } else {
                    return Err(CircuitBreakerError::CircuitOpen);
                }
            },
            CircuitState::HalfOpen => {},
            CircuitState::Closed => {},
        }
        
        match f() {
            Ok(result) => {
                self.record_success();
                Ok(result)
            },
            Err(error) => {
                self.record_failure();
                Err(CircuitBreakerError::CallFailed(error))
            }
        }
    }
    
    fn get_state(&self) -> CircuitState {
        match self.state.load(Ordering::Relaxed) {
            0 => CircuitState::Closed,
            1 => CircuitState::Open,
            2 => CircuitState::HalfOpen,
            _ => CircuitState::Closed,
        }
    }
    
    fn set_state(&self, state: CircuitState) {
        let value = match state {
            CircuitState::Closed => 0,
            CircuitState::Open => 1,
            CircuitState::HalfOpen => 2,
        };
        self.state.store(value, Ordering::Relaxed);
    }
    
    fn should_try(&self) -> bool {
        let last_failure = self.last_failure.load(Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        
        now - last_failure > self.timeout.as_millis() as u64
    }
    
    fn record_success(&self) {
        let success_count = self.success_count.fetch_add(1, Ordering::Relaxed) + 1;
        
        match self.get_state() {
            CircuitState::HalfOpen => {
                if success_count >= self.success_threshold {
                    self.set_state(CircuitState::Closed);
                    self.failure_count.store(0, Ordering::Relaxed);
                    self.success_count.store(0, Ordering::Relaxed);
                }
            },
            _ => {}
        }
    }
    
    fn record_failure(&self) {
        let failure_count = self.failure_count.fetch_add(1, Ordering::Relaxed) + 1;
        self.last_failure.store(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            Ordering::Relaxed
        );
        
        if failure_count >= self.failure_threshold {
            self.set_state(CircuitState::Open);
            self.success_count.store(0, Ordering::Relaxed);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug)]
pub enum CircuitBreakerError<E> {
    CircuitOpen,
    CallFailed(E),
}

impl<E: std::fmt::Display> std::fmt::Display for CircuitBreakerError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CircuitBreakerError::CircuitOpen => write!(f, "Circuit breaker is open"),
            CircuitBreakerError::CallFailed(e) => write!(f, "Call failed: {}", e),
        }
    }
}

impl<E: std::error::Error> std::error::Error for CircuitBreakerError<E> {}