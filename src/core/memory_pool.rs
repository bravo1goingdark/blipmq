//! High-performance memory pool for efficient message allocation
//!
//! This module provides object pooling capabilities to reduce allocation overhead
//! and improve cache locality for frequently allocated objects like messages,
//! buffers, and other temporary data structures.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use parking_lot::Mutex;
use bytes::BytesMut;

/// Statistics for a memory pool
#[derive(Debug, Default)]
pub struct PoolStats {
    /// Total allocations requested
    pub allocations: AtomicUsize,
    /// Total objects returned to pool
    pub deallocations: AtomicUsize,
    /// Cache hits (objects reused from pool)
    pub cache_hits: AtomicUsize,
    /// Cache misses (new objects created)
    pub cache_misses: AtomicUsize,
    /// Current pool size
    pub pool_size: AtomicUsize,
    /// Peak pool size
    pub peak_pool_size: AtomicUsize,
}

impl PoolStats {
    /// Returns the cache hit ratio as a percentage
    pub fn hit_ratio(&self) -> f64 {
        let hits = self.cache_hits.load(Ordering::Relaxed) as f64;
        let total = self.allocations.load(Ordering::Relaxed) as f64;
        if total > 0.0 { hits / total * 100.0 } else { 0.0 }
    }
    
    /// Returns current statistics snapshot
    pub fn snapshot(&self) -> PoolStatsSnapshot {
        PoolStatsSnapshot {
            allocations: self.allocations.load(Ordering::Relaxed),
            deallocations: self.deallocations.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            pool_size: self.pool_size.load(Ordering::Relaxed),
            peak_pool_size: self.peak_pool_size.load(Ordering::Relaxed),
            hit_ratio: self.hit_ratio(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PoolStatsSnapshot {
    pub allocations: usize,
    pub deallocations: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub pool_size: usize,
    pub peak_pool_size: usize,
    pub hit_ratio: f64,
}

/// Generic object pool implementation
pub struct ObjectPool<T> {
    pool: Mutex<VecDeque<T>>,
    factory: Box<dyn Fn() -> T + Send + Sync>,
    reset: Box<dyn Fn(&mut T) + Send + Sync>,
    max_size: usize,
    stats: PoolStats,
}

impl<T> ObjectPool<T> {
    /// Creates a new object pool with specified factory and reset functions
    pub fn new<F, R>(max_size: usize, factory: F, reset: R) -> Self
    where
        F: Fn() -> T + Send + Sync + 'static,
        R: Fn(&mut T) + Send + Sync + 'static,
    {
        Self {
            pool: Mutex::new(VecDeque::new()),
            factory: Box::new(factory),
            reset: Box::new(reset),
            max_size,
            stats: PoolStats::default(),
        }
    }
    
    /// Gets an object from the pool or creates a new one
    pub fn get(&self) -> PooledObject<T> {
        self.stats.allocations.fetch_add(1, Ordering::Relaxed);
        
        let mut pool = self.pool.lock();
        if let Some(mut object) = pool.pop_front() {
            // Reset the object before returning
            (self.reset)(&mut object);
            
            self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
            self.stats.pool_size.fetch_sub(1, Ordering::Relaxed);
            
            PooledObject::new(object, self)
        } else {
            // Create new object
            let object = (self.factory)();
            self.stats.cache_misses.fetch_add(1, Ordering::Relaxed);
            PooledObject::new(object, self)
        }
    }
    
    /// Returns an object to the pool
    fn return_object(&self, object: T) {
        self.stats.deallocations.fetch_add(1, Ordering::Relaxed);
        
        let mut pool = self.pool.lock();
        if pool.len() < self.max_size {
            pool.push_back(object);
            let new_size = pool.len();
            drop(pool);
            
            self.stats.pool_size.store(new_size, Ordering::Relaxed);
            
            // Update peak size if necessary
            let current_peak = self.stats.peak_pool_size.load(Ordering::Relaxed);
            if new_size > current_peak {
                self.stats.peak_pool_size.store(new_size, Ordering::Relaxed);
            }
        }
        // Object is dropped if pool is full
    }
    
    /// Returns current pool statistics
    pub fn stats(&self) -> PoolStatsSnapshot {
        self.stats.snapshot()
    }
    
    /// Clears the pool and resets statistics
    pub fn clear(&self) {
        let mut pool = self.pool.lock();
        pool.clear();
        drop(pool);
        
        self.stats.pool_size.store(0, Ordering::Relaxed);
        // Note: We don't reset other statistics as they represent historical data
    }
    
    /// Pre-fills the pool with objects
    pub fn warm_up(&self, count: usize) {
        let count = count.min(self.max_size);
        let mut pool = self.pool.lock();
        
        for _ in 0..count {
            if pool.len() >= self.max_size {
                break;
            }
            pool.push_back((self.factory)());
        }
        
        let new_size = pool.len();
        drop(pool);
        
        self.stats.pool_size.store(new_size, Ordering::Relaxed);
        self.stats.peak_pool_size.store(new_size, Ordering::Relaxed);
    }
}

/// RAII wrapper for pooled objects
pub struct PooledObject<T> {
    object: Option<T>,
    pool: *const ObjectPool<T>,
}

impl<T> PooledObject<T> {
    fn new(object: T, pool: &ObjectPool<T>) -> Self {
        Self {
            object: Some(object),
            pool: pool as *const ObjectPool<T>,
        }
    }
}

impl<T> std::ops::Deref for PooledObject<T> {
    type Target = T;
    
    fn deref(&self) -> &Self::Target {
        self.object.as_ref().unwrap()
    }
}

impl<T> std::ops::DerefMut for PooledObject<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.object.as_mut().unwrap()
    }
}

impl<T> Drop for PooledObject<T> {
    fn drop(&mut self) {
        if let Some(object) = self.object.take() {
            unsafe {
                // Safety: pool pointer is valid for the lifetime of this object
                (*self.pool).return_object(object);
            }
        }
    }
}

unsafe impl<T> Send for PooledObject<T> where T: Send {}
unsafe impl<T> Sync for PooledObject<T> where T: Sync {}

/// Pre-configured pools for common BlipMQ objects
pub struct MemoryPools {
    /// Pool for BytesMut buffers (various sizes)
    pub buffer_small: ObjectPool<BytesMut>,   // 1KB
    pub buffer_medium: ObjectPool<BytesMut>,  // 4KB  
    pub buffer_large: ObjectPool<BytesMut>,   // 16KB
    
    /// Pool for Vec<u8> buffers
    pub vec_small: ObjectPool<Vec<u8>>,       // 1KB capacity
    pub vec_medium: ObjectPool<Vec<u8>>,      // 4KB capacity
    
    /// Pool for message processing vectors
    pub message_batch: ObjectPool<Vec<crate::core::message::WireMessage>>,
}

impl MemoryPools {
    /// Creates new memory pools with optimized defaults
    pub fn new() -> Self {
        Self {
            buffer_small: ObjectPool::new(
                512,  // max pool size
                || BytesMut::with_capacity(1024),
                |buf| buf.clear(),
            ),
            buffer_medium: ObjectPool::new(
                256,
                || BytesMut::with_capacity(4096),
                |buf| buf.clear(),
            ),
            buffer_large: ObjectPool::new(
                128,
                || BytesMut::with_capacity(16384),
                |buf| buf.clear(),
            ),
            vec_small: ObjectPool::new(
                512,
                || Vec::with_capacity(1024),
                |vec| vec.clear(),
            ),
            vec_medium: ObjectPool::new(
                256,
                || Vec::with_capacity(4096),
                |vec| vec.clear(),
            ),
            message_batch: ObjectPool::new(
                128,
                || Vec::with_capacity(100),
                |vec| vec.clear(),
            ),
        }
    }
    
    /// Warms up all pools with initial objects
    pub fn warm_up(&self) {
        self.buffer_small.warm_up(64);
        self.buffer_medium.warm_up(32);
        self.buffer_large.warm_up(16);
        self.vec_small.warm_up(64);
        self.vec_medium.warm_up(32);
        self.message_batch.warm_up(16);
    }
    
    /// Returns combined statistics for all pools
    pub fn combined_stats(&self) -> CombinedPoolStats {
        CombinedPoolStats {
            buffer_small: self.buffer_small.stats(),
            buffer_medium: self.buffer_medium.stats(),
            buffer_large: self.buffer_large.stats(),
            vec_small: self.vec_small.stats(),
            vec_medium: self.vec_medium.stats(),
            message_batch: self.message_batch.stats(),
        }
    }
    
    /// Clears all pools
    pub fn clear_all(&self) {
        self.buffer_small.clear();
        self.buffer_medium.clear();
        self.buffer_large.clear();
        self.vec_small.clear();
        self.vec_medium.clear();
        self.message_batch.clear();
    }
}

impl Default for MemoryPools {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct CombinedPoolStats {
    pub buffer_small: PoolStatsSnapshot,
    pub buffer_medium: PoolStatsSnapshot,
    pub buffer_large: PoolStatsSnapshot,
    pub vec_small: PoolStatsSnapshot,
    pub vec_medium: PoolStatsSnapshot,
    pub message_batch: PoolStatsSnapshot,
}

impl CombinedPoolStats {
    /// Returns total allocations across all pools
    pub fn total_allocations(&self) -> usize {
        self.buffer_small.allocations +
        self.buffer_medium.allocations +
        self.buffer_large.allocations +
        self.vec_small.allocations +
        self.vec_medium.allocations +
        self.message_batch.allocations
    }
    
    /// Returns overall hit ratio across all pools
    pub fn overall_hit_ratio(&self) -> f64 {
        let total_hits = self.buffer_small.cache_hits +
                        self.buffer_medium.cache_hits +
                        self.buffer_large.cache_hits +
                        self.vec_small.cache_hits +
                        self.vec_medium.cache_hits +
                        self.message_batch.cache_hits;
                        
        let total_allocs = self.total_allocations();
        
        if total_allocs > 0 {
            total_hits as f64 / total_allocs as f64 * 100.0
        } else {
            0.0
        }
    }
}

/// Global memory pools instance
static GLOBAL_POOLS: once_cell::sync::Lazy<MemoryPools> = once_cell::sync::Lazy::new(|| {
    let pools = MemoryPools::new();
    pools.warm_up();
    pools
});

/// Get a reference to the global memory pools
pub fn global_pools() -> &'static MemoryPools {
    &GLOBAL_POOLS
}

/// Convenience functions for common allocations
pub mod convenience {
    use super::*;
    
    /// Get a small buffer (1KB capacity) from the global pool
    pub fn get_small_buffer() -> PooledObject<BytesMut> {
        global_pools().buffer_small.get()
    }
    
    /// Get a medium buffer (4KB capacity) from the global pool
    pub fn get_medium_buffer() -> PooledObject<BytesMut> {
        global_pools().buffer_medium.get()
    }
    
    /// Get a large buffer (16KB capacity) from the global pool
    pub fn get_large_buffer() -> PooledObject<BytesMut> {
        global_pools().buffer_large.get()
    }
    
    /// Get a small vector (1KB capacity) from the global pool
    pub fn get_small_vec() -> PooledObject<Vec<u8>> {
        global_pools().vec_small.get()
    }
    
    /// Get a medium vector (4KB capacity) from the global pool
    pub fn get_medium_vec() -> PooledObject<Vec<u8>> {
        global_pools().vec_medium.get()
    }
    
    /// Get a message batch vector from the global pool
    pub fn get_message_batch() -> PooledObject<Vec<crate::core::message::WireMessage>> {
        global_pools().message_batch.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_object_pool_basic() {
        let pool = ObjectPool::new(
            10,
            || Vec::<i32>::with_capacity(100),
            |v| v.clear(),
        );
        
        // Get an object
        let mut obj = pool.get();
        obj.push(42);
        assert_eq!(obj.len(), 1);
        
        // Drop it back to pool
        drop(obj);
        
        // Get another object - should be reused and cleared
        let obj2 = pool.get();
        assert_eq!(obj2.len(), 0);
        assert_eq!(obj2.capacity(), 100);
        
        let stats = pool.stats();
        assert_eq!(stats.allocations, 2);
        assert_eq!(stats.cache_hits, 1);
        assert_eq!(stats.cache_misses, 1);
    }
    
    #[test]
    fn test_pool_max_size() {
        let pool = ObjectPool::new(
            2,  // Small max size
            || Vec::<i32>::new(),
            |v| v.clear(),
        );
        
        // Create more objects than pool size
        let obj1 = pool.get();
        let obj2 = pool.get();
        let obj3 = pool.get();
        
        drop(obj1);
        drop(obj2);
        drop(obj3);
        
        let stats = pool.stats();
        assert_eq!(stats.pool_size, 2); // Only 2 should be retained
        assert_eq!(stats.deallocations, 3);
    }
    
    #[test]
    fn test_memory_pools() {
        let pools = MemoryPools::new();
        
        // Test buffer allocation
        let mut buffer = pools.buffer_small.get();
        buffer.extend_from_slice(b"hello");
        assert_eq!(buffer.len(), 5);
        drop(buffer);
        
        // Test reuse
        let buffer2 = pools.buffer_small.get();
        assert_eq!(buffer2.len(), 0); // Should be cleared
        
        let stats = pools.combined_stats();
        assert_eq!(stats.buffer_small.allocations, 2);
        assert_eq!(stats.buffer_small.cache_hits, 1);
    }
    
    #[test]
    fn test_convenience_functions() {
        use super::convenience::*;
        
        let mut buf = get_small_buffer();
        buf.extend_from_slice(b"test");
        assert_eq!(buf.len(), 4);
        assert!(buf.capacity() >= 1024);
    }
    
    #[test]
    fn test_pool_stats() {
        let pool = ObjectPool::new(
            5,
            || Vec::<i32>::new(),
            |v| v.clear(),
        );
        
        // Generate some activity
        for _ in 0..10 {
            let obj = pool.get();
            drop(obj);
        }
        
        let stats = pool.stats();
        assert_eq!(stats.allocations, 10);
        assert_eq!(stats.deallocations, 10);
        assert!(stats.hit_ratio > 0.0);
        // Pool size may be less than max due to timing of drops
        assert!(stats.pool_size <= 5);
    }
}