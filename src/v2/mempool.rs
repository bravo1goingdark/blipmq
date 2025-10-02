//! High-performance memory pool with huge pages support
//! 
//! Pre-allocated memory pools to eliminate allocation overhead

use std::alloc::{alloc, dealloc, Layout};
use std::sync::atomic::{AtomicUsize, AtomicPtr, Ordering};
use std::ptr::{self, NonNull};
use std::mem::{self, MaybeUninit};
use parking_lot::Mutex;

/// Size classes for the memory pool
const SIZE_CLASSES: &[usize] = &[
    64,    // Small messages
    256,   // Medium messages  
    1024,  // Large messages
    4096,  // Extra large
    16384, // Jumbo
];

/// Cache line size
const CACHE_LINE: usize = 64;

/// Huge page size (2MB on x86_64)
const HUGE_PAGE_SIZE: usize = 2 * 1024 * 1024;

/// Aligned allocation for cache efficiency
#[repr(align(64))]
struct CacheAligned<T>(T);

/// Memory block header
#[repr(C)]
struct BlockHeader {
    next: AtomicPtr<BlockHeader>,
    size: usize,
    pool_id: usize,
}

/// Per-thread memory pool to avoid contention
pub struct MemoryPool {
    /// Free lists for each size class
    free_lists: Vec<CacheAligned<AtomicPtr<BlockHeader>>>,
    /// Statistics
    allocated: AtomicUsize,
    freed: AtomicUsize,
    /// Huge page arena
    huge_page_arena: Option<HugePageArena>,
}

impl MemoryPool {
    /// Create a new memory pool
    pub fn new() -> Self {
        let mut free_lists = Vec::with_capacity(SIZE_CLASSES.len());
        for _ in SIZE_CLASSES {
            free_lists.push(CacheAligned(AtomicPtr::new(ptr::null_mut())));
        }
        
        // Try to allocate huge page arena
        let huge_page_arena = HugePageArena::new().ok();
        
        Self {
            free_lists,
            allocated: AtomicUsize::new(0),
            freed: AtomicUsize::new(0),
            huge_page_arena,
        }
    }
    
    /// Allocate memory from the pool
    #[inline(always)]
    pub fn allocate(&self, size: usize) -> Option<NonNull<u8>> {
        // Find appropriate size class
        let class_idx = self.size_class(size)?;
        let class_size = SIZE_CLASSES[class_idx];
        
        // Try to get from free list first
        let free_list = &self.free_lists[class_idx].0;
        let mut head = free_list.load(Ordering::Acquire);
        
        loop {
            if head.is_null() {
                // Allocate new block
                return self.allocate_new_block(class_size, class_idx);
            }
            
            let next = unsafe { (*head).next.load(Ordering::Acquire) };
            
            match free_list.compare_exchange_weak(
                head,
                next,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.allocated.fetch_add(1, Ordering::Relaxed);
                    let ptr = head as *mut u8;
                    unsafe { ptr.add(mem::size_of::<BlockHeader>()) };
                    return NonNull::new(unsafe { ptr.add(mem::size_of::<BlockHeader>()) });
                }
                Err(actual) => head = actual,
            }
        }
    }
    
    /// Free memory back to the pool
    #[inline(always)]
    pub fn free(&self, ptr: NonNull<u8>, size: usize) {
        let class_idx = match self.size_class(size) {
            Some(idx) => idx,
            None => return, // Invalid size
        };
        
        // Get block header
        let header_ptr = unsafe {
            ptr.as_ptr().sub(mem::size_of::<BlockHeader>()) as *mut BlockHeader
        };
        
        // Add to free list
        let free_list = &self.free_lists[class_idx].0;
        let mut head = free_list.load(Ordering::Acquire);
        
        loop {
            unsafe {
                (*header_ptr).next.store(head, Ordering::Release);
            }
            
            match free_list.compare_exchange_weak(
                head,
                header_ptr,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.freed.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(actual) => head = actual,
            }
        }
    }
    
    /// Find size class for given size
    #[inline(always)]
    fn size_class(&self, size: usize) -> Option<usize> {
        SIZE_CLASSES.iter().position(|&s| s >= size)
    }
    
    /// Allocate a new block
    fn allocate_new_block(&self, size: usize, class_idx: usize) -> Option<NonNull<u8>> {
        // Try huge page arena first
        if let Some(ref arena) = self.huge_page_arena {
            if let Some(ptr) = arena.allocate(size + mem::size_of::<BlockHeader>()) {
                let header = ptr.as_ptr() as *mut BlockHeader;
                unsafe {
                    (*header).next.store(ptr::null_mut(), Ordering::Relaxed);
                    (*header).size = size;
                    (*header).pool_id = class_idx;
                }
                
                self.allocated.fetch_add(1, Ordering::Relaxed);
                return NonNull::new(unsafe { ptr.as_ptr().add(mem::size_of::<BlockHeader>()) });
            }
        }
        
        // Fallback to system allocator
        let layout = Layout::from_size_align(
            size + mem::size_of::<BlockHeader>(),
            CACHE_LINE,
        ).ok()?;
        
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            return None;
        }
        
        let header = ptr as *mut BlockHeader;
        unsafe {
            (*header).next.store(ptr::null_mut(), Ordering::Relaxed);
            (*header).size = size;
            (*header).pool_id = class_idx;
        }
        
        self.allocated.fetch_add(1, Ordering::Relaxed);
        NonNull::new(unsafe { ptr.add(mem::size_of::<BlockHeader>()) })
    }
    
    /// Get statistics
    pub fn stats(&self) -> (usize, usize) {
        (
            self.allocated.load(Ordering::Relaxed),
            self.freed.load(Ordering::Relaxed),
        )
    }
}

/// Huge page arena for reduced TLB misses
struct HugePageArena {
    base: NonNull<u8>,
    size: usize,
    offset: AtomicUsize,
}

impl HugePageArena {
    /// Try to allocate huge page arena
    fn new() -> Result<Self, &'static str> {
        #[cfg(target_os = "linux")]
        {
            use std::os::raw::c_void;
            
            // Try to allocate with huge pages
            let size = 16 * HUGE_PAGE_SIZE; // 32MB arena
            let ptr = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB,
                    -1,
                    0,
                )
            };
            
            if ptr == libc::MAP_FAILED {
                // Fallback to regular pages with madvise
                let ptr = unsafe {
                    libc::mmap(
                        ptr::null_mut(),
                        size,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    )
                };
                
                if ptr == libc::MAP_FAILED {
                    return Err("Failed to allocate arena");
                }
                
                // Advise kernel to use huge pages
                unsafe {
                    libc::madvise(ptr, size, libc::MADV_HUGEPAGE);
                }
            }
            
            Ok(Self {
                base: NonNull::new(ptr as *mut u8).unwrap(),
                size,
                offset: AtomicUsize::new(0),
            })
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            // Regular allocation on non-Linux
            let layout = Layout::from_size_align(16 * HUGE_PAGE_SIZE, HUGE_PAGE_SIZE)
                .map_err(|_| "Invalid layout")?;
            
            let ptr = unsafe { alloc(layout) };
            if ptr.is_null() {
                return Err("Failed to allocate arena");
            }
            
            Ok(Self {
                base: NonNull::new(ptr).unwrap(),
                size: 16 * HUGE_PAGE_SIZE,
                offset: AtomicUsize::new(0),
            })
        }
    }
    
    /// Allocate from arena
    #[inline(always)]
    fn allocate(&self, size: usize) -> Option<NonNull<u8>> {
        let aligned_size = (size + CACHE_LINE - 1) & !(CACHE_LINE - 1);
        let offset = self.offset.fetch_add(aligned_size, Ordering::Relaxed);
        
        if offset + aligned_size > self.size {
            return None;
        }
        
        Some(unsafe { NonNull::new_unchecked(self.base.as_ptr().add(offset)) })
    }
}

impl Drop for HugePageArena {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            unsafe {
                libc::munmap(self.base.as_ptr() as *mut libc::c_void, self.size);
            }
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            let layout = Layout::from_size_align(self.size, HUGE_PAGE_SIZE).unwrap();
            unsafe {
                dealloc(self.base.as_ptr(), layout);
            }
        }
    }
}

/// Thread-local memory pool
thread_local! {
    static THREAD_POOL: MemoryPool = MemoryPool::new();
}

/// Allocate from thread-local pool
#[inline(always)]
pub fn pool_alloc(size: usize) -> Option<NonNull<u8>> {
    THREAD_POOL.with(|pool| pool.allocate(size))
}

/// Free to thread-local pool
#[inline(always)]
pub fn pool_free(ptr: NonNull<u8>, size: usize) {
    THREAD_POOL.with(|pool| pool.free(ptr, size))
}

/// Global pool for shared allocations
static GLOBAL_POOL: Lazy<MemoryPool> = Lazy::new(|| MemoryPool::new());

use once_cell::sync::Lazy;

/// Allocate from global pool
#[inline(always)]
pub fn global_alloc(size: usize) -> Option<NonNull<u8>> {
    GLOBAL_POOL.allocate(size)
}

/// Free to global pool
#[inline(always)]
pub fn global_free(ptr: NonNull<u8>, size: usize) {
    GLOBAL_POOL.free(ptr, size)
}
