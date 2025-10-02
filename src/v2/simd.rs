//! SIMD operations for ultra-fast batch processing
//! 
//! Uses x86_64 AVX2/AVX512 and ARM NEON for parallel operations

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

use std::mem;

/// SIMD-accelerated memory copy
#[inline(always)]
pub unsafe fn simd_memcpy(dst: *mut u8, src: *const u8, len: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            avx2_memcpy(dst, src, len);
        } else if is_x86_feature_detected!("sse4.2") {
            sse_memcpy(dst, src, len);
        } else {
            std::ptr::copy_nonoverlapping(src, dst, len);
        }
    }
    
    #[cfg(not(target_arch = "x86_64"))]
    {
        std::ptr::copy_nonoverlapping(src, dst, len);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn avx2_memcpy(mut dst: *mut u8, mut src: *const u8, mut len: usize) {
    // Copy 32-byte chunks with AVX2
    while len >= 32 {
        let data = _mm256_loadu_si256(src as *const __m256i);
        _mm256_storeu_si256(dst as *mut __m256i, data);
        dst = dst.add(32);
        src = src.add(32);
        len -= 32;
    }
    
    // Copy remaining bytes
    if len > 0 {
        std::ptr::copy_nonoverlapping(src, dst, len);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn sse_memcpy(mut dst: *mut u8, mut src: *const u8, mut len: usize) {
    // Copy 16-byte chunks with SSE
    while len >= 16 {
        let data = _mm_loadu_si128(src as *const __m128i);
        _mm_storeu_si128(dst as *mut __m128i, data);
        dst = dst.add(16);
        src = src.add(16);
        len -= 16;
    }
    
    // Copy remaining bytes
    if len > 0 {
        std::ptr::copy_nonoverlapping(src, dst, len);
    }
}

/// SIMD-accelerated pattern search (find delimiter)
#[inline(always)]
pub unsafe fn simd_find_delimiter(data: &[u8], delimiter: u8) -> Option<usize> {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            return avx2_find_byte(data, delimiter);
        } else if is_x86_feature_detected!("sse4.2") {
            return sse_find_byte(data, delimiter);
        }
    }
    
    // Fallback to standard search
    data.iter().position(|&b| b == delimiter)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn avx2_find_byte(data: &[u8], needle: u8) -> Option<usize> {
    let len = data.len();
    let ptr = data.as_ptr();
    
    // Create a vector with the needle byte repeated
    let needle_vec = _mm256_set1_epi8(needle as i8);
    
    let mut offset = 0;
    
    // Process 32 bytes at a time
    while offset + 32 <= len {
        let chunk = _mm256_loadu_si256(ptr.add(offset) as *const __m256i);
        let cmp = _mm256_cmpeq_epi8(chunk, needle_vec);
        let mask = _mm256_movemask_epi8(cmp);
        
        if mask != 0 {
            return Some(offset + mask.trailing_zeros() as usize);
        }
        
        offset += 32;
    }
    
    // Check remaining bytes
    for i in offset..len {
        if data[i] == needle {
            return Some(i);
        }
    }
    
    None
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn sse_find_byte(data: &[u8], needle: u8) -> Option<usize> {
    let len = data.len();
    let ptr = data.as_ptr();
    
    // Create a vector with the needle byte repeated
    let needle_vec = _mm_set1_epi8(needle as i8);
    
    let mut offset = 0;
    
    // Process 16 bytes at a time
    while offset + 16 <= len {
        let chunk = _mm_loadu_si128(ptr.add(offset) as *const __m128i);
        let cmp = _mm_cmpeq_epi8(chunk, needle_vec);
        let mask = _mm_movemask_epi8(cmp);
        
        if mask != 0 {
            return Some(offset + mask.trailing_zeros() as usize);
        }
        
        offset += 16;
    }
    
    // Check remaining bytes
    for i in offset..len {
        if data[i] == needle {
            return Some(i);
        }
    }
    
    None
}

/// SIMD-accelerated CRC32 checksum
#[inline(always)]
pub fn simd_crc32(data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("sse4.2") {
            return unsafe { sse42_crc32(data) };
        }
    }
    
    // Fallback to standard CRC32
    crc32_fallback(data)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn sse42_crc32(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    let mut ptr = data.as_ptr();
    let mut len = data.len();
    
    // Process 8 bytes at a time
    while len >= 8 {
        let chunk = *(ptr as *const u64);
        crc = _mm_crc32_u64(crc as u64, chunk) as u32;
        ptr = ptr.add(8);
        len -= 8;
    }
    
    // Process 4 bytes
    if len >= 4 {
        let chunk = *(ptr as *const u32);
        crc = _mm_crc32_u32(crc, chunk);
        ptr = ptr.add(4);
        len -= 4;
    }
    
    // Process remaining bytes
    while len > 0 {
        crc = _mm_crc32_u8(crc, *ptr);
        ptr = ptr.add(1);
        len -= 1;
    }
    
    crc
}

fn crc32_fallback(data: &[u8]) -> u32 {
    // Simple CRC32 fallback
    let mut crc = 0xFFFFFFFF_u32;
    for &byte in data {
        crc = crc ^ (byte as u32);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB88320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// SIMD-accelerated batch message validation
#[inline(always)]
pub fn simd_validate_messages(messages: &[&[u8]], min_len: usize) -> Vec<bool> {
    let mut results = Vec::with_capacity(messages.len());
    
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            unsafe {
                return avx2_validate_batch(messages, min_len);
            }
        }
    }
    
    // Fallback
    for msg in messages {
        results.push(msg.len() >= min_len);
    }
    results
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn avx2_validate_batch(messages: &[&[u8]], min_len: usize) -> Vec<bool> {
    let mut results = Vec::with_capacity(messages.len());
    
    // Process 8 messages at a time
    let chunks = messages.chunks_exact(8);
    let remainder = chunks.remainder();
    
    for chunk in chunks {
        // Load lengths into AVX register
        let lengths = _mm256_set_epi32(
            chunk[7].len() as i32,
            chunk[6].len() as i32,
            chunk[5].len() as i32,
            chunk[4].len() as i32,
            chunk[3].len() as i32,
            chunk[2].len() as i32,
            chunk[1].len() as i32,
            chunk[0].len() as i32,
        );
        
        let min_lens = _mm256_set1_epi32(min_len as i32);
        let cmp = _mm256_cmpgt_epi32(lengths, min_lens);
        let mask = _mm256_movemask_epi8(cmp);
        
        // Extract results
        for i in 0..8 {
            results.push((mask >> (i * 4)) & 0xF == 0xF);
        }
    }
    
    // Process remainder
    for msg in remainder {
        results.push(msg.len() >= min_len);
    }
    
    results
}

/// Prefetch data into cache
#[inline(always)]
pub fn prefetch_read(ptr: *const u8) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        _mm_prefetch(ptr as *const i8, _MM_HINT_T0);
    }
}

/// Prefetch for write
#[inline(always)]
pub fn prefetch_write(ptr: *const u8) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        _mm_prefetch(ptr as *const i8, _MM_HINT_T0);
    }
}

/// Likely branch hint
#[inline(always)]
pub fn likely(b: bool) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        if b { 
            unsafe { std::intrinsics::likely(true) }
        } else { 
            unsafe { std::intrinsics::unlikely(false) }
        }
    }
    
    #[cfg(not(target_arch = "x86_64"))]
    b
}
