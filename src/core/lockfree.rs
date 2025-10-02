//! Lock-free data structures for ultra-low latency
//!
//! Custom implementations optimized for message passing workloads

use std::sync::atomic::{AtomicUsize, AtomicPtr, Ordering};
use std::ptr;
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::alloc::{alloc, dealloc, Layout};
use bytes::Bytes;

/// Cache line size for padding (64 bytes on x86_64)
const CACHE_LINE: usize = 64;

/// Padding to prevent false sharing
#[repr(align(64))]
struct CachePadded<T>(T);

/// Ultra-fast MPMC queue using ring buffer
/// 
/// Features:
/// - Lock-free using CAS operations
/// - Cache-line padded to prevent false sharing
/// - Power-of-2 sizing for fast modulo
/// - Bounded capacity with backpressure
pub struct MpmcQueue<T> {
    /// Ring buffer storage
    buffer: *mut UnsafeCell<MaybeUninit<T>>,
    /// Capacity (must be power of 2)
    capacity: usize,
    /// Mask for fast modulo (capacity - 1)
    mask: usize,
    /// Head position (enqueue)
    head: CachePadded<AtomicUsize>,
    /// Tail position (dequeue)
    tail: CachePadded<AtomicUsize>,
    /// Cached head for consumers
    cached_head: CachePadded<AtomicUsize>,
    /// Cached tail for producers
    cached_tail: CachePadded<AtomicUsize>,
}

unsafe impl<T: Send> Send for MpmcQueue<T> {}
unsafe impl<T: Send> Sync for MpmcQueue<T> {}

impl<T> MpmcQueue<T> {
    /// Create new queue with given capacity (rounded up to power of 2)
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.next_power_of_two();
        let mask = capacity - 1;
        
        // Allocate aligned buffer
        let layout = Layout::array::<UnsafeCell<MaybeUninit<T>>>(capacity).unwrap();
        let buffer = unsafe { alloc(layout) as *mut UnsafeCell<MaybeUninit<T>> };
        
        if buffer.is_null() {
            panic!("Failed to allocate queue buffer");
        }
        
        Self {
            buffer,
            capacity,
            mask,
            head: CachePadded(AtomicUsize::new(0)),
            tail: CachePadded(AtomicUsize::new(0)),
            cached_head: CachePadded(AtomicUsize::new(0)),
            cached_tail: CachePadded(AtomicUsize::new(0)),
        }
    }
    
    /// Try to enqueue item (non-blocking)
    #[inline(always)]
    pub fn try_enqueue(&self, item: T) -> Result<(), T> {
        let mut head = self.head.0.load(Ordering::Relaxed);
        
        loop {
            let tail = self.cached_tail.0.load(Ordering::Relaxed);
            
            // Check if full
            if head.wrapping_sub(tail) >= self.capacity {
                // Update cached tail
                let actual_tail = self.tail.0.load(Ordering::Acquire);
                self.cached_tail.0.store(actual_tail, Ordering::Relaxed);
                
                if head.wrapping_sub(actual_tail) >= self.capacity {
                    return Err(item);
                }
            }
            
            // Try to claim slot
            match self.head.0.compare_exchange_weak(
                head,
                head.wrapping_add(1),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // Write item
                    unsafe {
                        let slot = self.buffer.add(head & self.mask);
                        (*slot).get().write(MaybeUninit::new(item));
                    }
                    
                    // Update cached head for consumers
                    self.cached_head.0.fetch_max(head.wrapping_add(1), Ordering::Release);
                    
                    return Ok(());
                }
                Err(actual) => head = actual,
            }
        }
    }
    
    /// Try to dequeue item (non-blocking)
    #[inline(always)]
    pub fn try_dequeue(&self) -> Option<T> {
        let mut tail = self.tail.0.load(Ordering::Relaxed);
        
        loop {
            let head = self.cached_head.0.load(Ordering::Relaxed);
            
            // Check if empty
            if tail >= head {
                // Update cached head
                let actual_head = self.head.0.load(Ordering::Acquire);
                self.cached_head.0.store(actual_head, Ordering::Relaxed);
                
                if tail >= actual_head {
                    return None;
                }
            }
            
            // Try to claim slot
            match self.tail.0.compare_exchange_weak(
                tail,
                tail.wrapping_add(1),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // Read item
                    let item = unsafe {
                        let slot = self.buffer.add(tail & self.mask);
                        (*slot).get().read().assume_init()
                    };
                    
                    // Update cached tail for producers
                    self.cached_tail.0.fetch_max(tail.wrapping_add(1), Ordering::Release);
                    
                    return Some(item);
                }
                Err(actual) => tail = actual,
            }
        }
    }
    
    /// Get approximate queue size
    #[inline(always)]
    pub fn len(&self) -> usize {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Relaxed);
        head.wrapping_sub(tail)
    }
    
    /// Check if queue is empty
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T> Drop for MpmcQueue<T> {
    fn drop(&mut self) {
        // Clean up remaining items
        while self.try_dequeue().is_some() {}
        
        // Deallocate buffer
        let layout = Layout::array::<UnsafeCell<MaybeUninit<T>>>(self.capacity).unwrap();
        unsafe {
            dealloc(self.buffer as *mut u8, layout);
        }
    }
}

/// Lock-free subscriber registry using atomic pointers
pub struct LockFreeRegistry<K: Eq + std::hash::Hash, V> {
    buckets: Vec<CachePadded<AtomicPtr<Node<K, V>>>>,
    size: AtomicUsize,
}

struct Node<K, V> {
    key: K,
    value: V,
    next: AtomicPtr<Node<K, V>>,
}

impl<K: Eq + std::hash::Hash, V> LockFreeRegistry<K, V> {
    pub fn new(num_buckets: usize) -> Self {
        let num_buckets = num_buckets.next_power_of_two();
        let mut buckets = Vec::with_capacity(num_buckets);
        
        for _ in 0..num_buckets {
            buckets.push(CachePadded(AtomicPtr::new(ptr::null_mut())));
        }
        
        Self {
            buckets,
            size: AtomicUsize::new(0),
        }
    }
    
    #[inline(always)]
    fn hash(&self, key: &K) -> usize {
        use std::hash::{Hash, Hasher};
        let mut hasher = ahash::AHasher::default();
        key.hash(&mut hasher);
        hasher.finish() as usize & (self.buckets.len() - 1)
    }
    
    /// Insert or update value
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let idx = self.hash(&key);
        let bucket = &self.buckets[idx].0;
        
        let new_node = Box::into_raw(Box::new(Node {
            key,
            value,
            next: AtomicPtr::new(ptr::null_mut()),
        }));
        
        loop {
            let head = bucket.load(Ordering::Acquire);
            unsafe {
                (*new_node).next.store(head, Ordering::Relaxed);
            }
            
            match bucket.compare_exchange_weak(
                head,
                new_node,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.size.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
                Err(_) => continue,
            }
        }
    }
    
    /// Get value by key
    pub fn get(&self, key: &K) -> Option<&V> {
        let idx = self.hash(key);
        let mut current = self.buckets[idx].0.load(Ordering::Acquire);
        
        while !current.is_null() {
            unsafe {
                if (*current).key == *key {
                    return Some(&(*current).value);
                }
                current = (*current).next.load(Ordering::Acquire);
            }
        }
        
        None
    }
    
    /// Remove value by key
    pub fn remove(&self, key: &K) -> Option<V> {
        let idx = self.hash(key);
        let bucket = &self.buckets[idx].0;
        let mut prev_ptr = bucket as *const _ as *mut AtomicPtr<Node<K, V>>;
        let mut current = bucket.load(Ordering::Acquire);
        
        while !current.is_null() {
            unsafe {
                if (*current).key == *key {
                    let next = (*current).next.load(Ordering::Acquire);
                    
                    match (*prev_ptr).compare_exchange(
                        current,
                        next,
                        Ordering::Release,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => {
                            let node = Box::from_raw(current);
                            self.size.fetch_sub(1, Ordering::Relaxed);
                            return Some(node.value);
                        }
                        Err(_) => return None,
                    }
                }
                
                prev_ptr = &mut (*current).next as *mut _;
                current = (*current).next.load(Ordering::Acquire);
            }
        }
        
        None
    }
    
    /// Get current size
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.size.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::sync::Arc;
    
    #[test]
    fn test_mpmc_queue() {
        let queue = Arc::new(MpmcQueue::new(1024));
        let num_producers = 4;
        let num_consumers = 4;
        let items_per_thread = 10000;
        
        let mut producers = vec![];
        let mut consumers = vec![];
        
        // Spawn producers
        for i in 0..num_producers {
            let q = queue.clone();
            producers.push(thread::spawn(move || {
                for j in 0..items_per_thread {
                    let val = i * items_per_thread + j;
                    while q.try_enqueue(val).is_err() {
                        std::hint::spin_loop();
                    }
                }
            }));
        }
        
        // Spawn consumers
        for _ in 0..num_consumers {
            let q = queue.clone();
            consumers.push(thread::spawn(move || {
                let mut count = 0;
                while count < (num_producers * items_per_thread / num_consumers) {
                    if q.try_dequeue().is_some() {
                        count += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            }));
        }
        
        // Wait for completion
        for p in producers {
            p.join().unwrap();
        }
        for c in consumers {
            c.join().unwrap();
        }
        
        assert_eq!(queue.len(), 0);
    }
}
