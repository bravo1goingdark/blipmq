//! High-performance memory pool for reducing allocations in hot paths.
//! 
//! Provides object pooling for frequently allocated types like buffers,
//! messages, and protocol frames to minimize GC pressure and improve performance.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use crossbeam_queue::SegQueue;
use bytes::{BytesMut, Bytes};
use once_cell::sync::Lazy;

/// Global memory pool statistics
#[derive(Debug, Default)]
pub struct PoolStats {
    pub buffer_pool_hits: AtomicUsize,
    pub buffer_pool_misses: AtomicUsize,
    pub message_pool_hits: AtomicUsize,  
    pub message_pool_misses: AtomicUsize,
}

pub static POOL_STATS: Lazy<PoolStats> = Lazy::new(Default::default);

/// Reusable buffer for protocol serialization/deserialization
#[derive(Debug)]
pub struct PooledBuffer {
    buffer: BytesMut,
    capacity: usize,
}

impl PooledBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            buffer: BytesMut::with_capacity(capacity),
            capacity,
        }
    }
    
    pub fn as_mut(&mut self) -> &mut BytesMut {
        &mut self.buffer
    }
    
    pub fn as_bytes(&self) -> &[u8] {
        &self.buffer
    }
    
    pub fn clear(&mut self) {
        self.buffer.clear();
        // If buffer grew too large, shrink it back to original capacity
        if self.buffer.capacity() > self.capacity * 2 {
            self.buffer = BytesMut::with_capacity(self.capacity);
        }
    }
    
    pub fn len(&self) -> usize {
        self.buffer.len()
    }
    
    pub fn freeze(self) -> Bytes {
        self.buffer.freeze()
    }
}

/// Thread-safe buffer pool using lock-free queue
#[derive(Debug)]
pub struct BufferPool {
    pool: SegQueue<PooledBuffer>,
    capacity: usize,
    max_pool_size: usize,
}

impl BufferPool {
    pub fn new(capacity: usize, max_pool_size: usize) -> Self {
        let pool = SegQueue::new();
        // Pre-populate with some buffers
        let initial_size = (max_pool_size / 4).max(1);
        for _ in 0..initial_size {
            pool.push(PooledBuffer::new(capacity));
        }
        
        Self {
            pool,
            capacity,
            max_pool_size,
        }
    }
    
    pub fn acquire(&self) -> PooledBuffer {
        if let Some(mut buffer) = self.pool.pop() {
            buffer.clear();
            POOL_STATS.buffer_pool_hits.fetch_add(1, Ordering::Relaxed);
            buffer
        } else {
            POOL_STATS.buffer_pool_misses.fetch_add(1, Ordering::Relaxed);
            PooledBuffer::new(self.capacity)
        }
    }
    
    pub fn release(&self, buffer: PooledBuffer) {
        // Only return to pool if we're not at capacity and buffer isn't too large
        if self.pool.len() < self.max_pool_size && buffer.buffer.capacity() <= self.capacity * 2 {
            self.pool.push(buffer);
        }
        // Otherwise, let it drop to free memory
    }
}

/// Global buffer pools for different use cases
pub static SMALL_BUFFER_POOL: Lazy<BufferPool> = Lazy::new(|| BufferPool::new(1024, 100));
pub static MEDIUM_BUFFER_POOL: Lazy<BufferPool> = Lazy::new(|| BufferPool::new(4096, 50));
pub static LARGE_BUFFER_POOL: Lazy<BufferPool> = Lazy::new(|| BufferPool::new(16384, 20));

/// RAII wrapper for automatic buffer return to pool
pub struct PooledBufferGuard {
    buffer: Option<PooledBuffer>,
    pool: &'static BufferPool,
}

impl PooledBufferGuard {
    fn new(buffer: PooledBuffer, pool: &'static BufferPool) -> Self {
        Self {
            buffer: Some(buffer),
            pool,
        }
    }
    
    pub fn as_mut(&mut self) -> &mut PooledBuffer {
        self.buffer.as_mut().unwrap()
    }
    
    pub fn as_ref(&self) -> &PooledBuffer {
        self.buffer.as_ref().unwrap()
    }
}

impl Drop for PooledBufferGuard {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            self.pool.release(buffer);
        }
    }
}

/// Convenient buffer acquisition functions
pub fn acquire_small_buffer() -> PooledBufferGuard {
    PooledBufferGuard::new(SMALL_BUFFER_POOL.acquire(), &SMALL_BUFFER_POOL)
}

pub fn acquire_medium_buffer() -> PooledBufferGuard {
    PooledBufferGuard::new(MEDIUM_BUFFER_POOL.acquire(), &MEDIUM_BUFFER_POOL)
}

pub fn acquire_large_buffer() -> PooledBufferGuard {
    PooledBufferGuard::new(LARGE_BUFFER_POOL.acquire(), &LARGE_BUFFER_POOL)
}

/// Message pool for reducing message allocation overhead
#[derive(Debug)]
pub struct MessagePool {
    pool: SegQueue<Box<crate::core::message::Message>>,
    max_pool_size: usize,
}

impl MessagePool {
    pub fn new(max_pool_size: usize) -> Self {
        Self {
            pool: SegQueue::new(),
            max_pool_size,
        }
    }
    
    pub fn acquire(&self) -> Box<crate::core::message::Message> {
        if let Some(mut msg) = self.pool.pop() {
            // Reset the message for reuse
            msg.id = 0;
            msg.payload.clear();
            msg.timestamp = 0;
            msg.ttl_ms = 0;
            POOL_STATS.message_pool_hits.fetch_add(1, Ordering::Relaxed);
            msg
        } else {
            POOL_STATS.message_pool_misses.fetch_add(1, Ordering::Relaxed);
            Box::new(crate::core::message::Message {
                id: 0,
                payload: Vec::new(),
                timestamp: 0,
                ttl_ms: 0,
            })
        }
    }
    
    pub fn release(&self, msg: Box<crate::core::message::Message>) {
        if self.pool.len() < self.max_pool_size {
            self.pool.push(msg);
        }
    }
}

pub static MESSAGE_POOL: Lazy<MessagePool> = Lazy::new(|| MessagePool::new(1000));

/// Zero-copy utilities for performance
pub mod zero_copy {
    use bytes::{Bytes, BytesMut};
    use std::io::IoSlice;
    
    /// Prepare vectored I/O slices for batch writing
    pub fn prepare_vectored_write(buffers: &[&[u8]]) -> Vec<IoSlice<'_>> {
        buffers.iter().map(|buf| IoSlice::new(buf)).collect()
    }
    
    /// Slice a buffer without copying
    pub fn slice_buffer(buffer: &Bytes, start: usize, end: usize) -> Bytes {
        buffer.slice(start..end)
    }
    
    /// Convert to shared bytes without copying
    pub fn to_shared_bytes(buffer: BytesMut) -> Bytes {
        buffer.freeze()
    }
}

/// Pool statistics reporting
impl PoolStats {
    pub fn buffer_hit_rate(&self) -> f64 {
        let hits = self.buffer_pool_hits.load(Ordering::Relaxed) as f64;
        let misses = self.buffer_pool_misses.load(Ordering::Relaxed) as f64;
        if hits + misses == 0.0 { 0.0 } else { hits / (hits + misses) }
    }
    
    pub fn message_hit_rate(&self) -> f64 {
        let hits = self.message_pool_hits.load(Ordering::Relaxed) as f64;
        let misses = self.message_pool_misses.load(Ordering::Relaxed) as f64;
        if hits + misses == 0.0 { 0.0 } else { hits / (hits + misses) }
    }
    
    pub fn reset(&self) {
        self.buffer_pool_hits.store(0, Ordering::Relaxed);
        self.buffer_pool_misses.store(0, Ordering::Relaxed);
        self.message_pool_hits.store(0, Ordering::Relaxed);
        self.message_pool_misses.store(0, Ordering::Relaxed);
    }
}