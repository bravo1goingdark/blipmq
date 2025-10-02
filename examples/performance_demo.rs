//! BlipMQ v1.0 Performance Demonstration
//!
//! This example showcases the high-performance features introduced in v1.0:
//! - Timer wheel for efficient TTL management
//! - Memory pools for reduced allocation overhead
//! - Batch processing for improved throughput
//! - Performance monitoring and statistics

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;

use blipmq::core::timer_wheel::TimerManager;
use blipmq::core::memory_pool::{global_pools, convenience::*};
use blipmq::core::batch_processor::{BatchConfig, TTLBatchProcessor};
use blipmq::core::message::{WireMessage, current_timestamp};
use bytes::Bytes;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing for better observability
    tracing_subscriber::fmt::init();
    
    println!("🚀 BlipMQ v1.0 Performance Demo");
    println!("================================");
    
    // Demo 1: Timer Wheel Performance
    demo_timer_wheel().await?;
    
    // Demo 2: Memory Pool Efficiency
    demo_memory_pools().await?;
    
    // Demo 3: Batch Processing
    demo_batch_processing().await?;
    
    // Demo 4: Combined Performance Statistics
    demo_performance_stats().await?;
    
    println!("\n✅ Performance demo completed successfully!");
    Ok(())
}

async fn demo_timer_wheel() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n📅 Timer Wheel Demo");
    println!("-------------------");
    
    let timer_manager = Arc::new(TimerManager::new());
    let now = current_timestamp();
    
    // Create messages with varying TTL values
    println!("Creating 10,000 messages with different TTL values...");
    let mut messages = Vec::new();
    
    for i in 0..10_000 {
        let ttl_offset = (i % 1000) as u64 * 10; // TTLs from 0ms to 10s
        let message = Arc::new(WireMessage {
            frame: Bytes::from(format!("message-{}", i)),
            expire_at: now + 1000 + ttl_offset, // Base 1s TTL + offset
        });
        
        timer_manager.schedule_expiration(message.clone())?;
        messages.push(message);
    }
    
    // Wait for some messages to expire
    println!("Waiting for TTL processing...");
    sleep(Duration::from_millis(500)).await;
    
    let (inserted, expired, cascaded) = timer_manager.stats();
    println!("Timer Statistics:");
    println!("  • Messages inserted: {}", inserted);
    println!("  • Messages expired:  {}", expired);
    println!("  • Timer cascades:    {}", cascaded);
    println!("  • Pending timers:    {}", timer_manager.pending_count());
    
    Ok(())
}

async fn demo_memory_pools() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n🏊 Memory Pool Demo");
    println!("-------------------");
    
    // Show initial pool statistics
    let initial_stats = global_pools().combined_stats();
    println!("Initial pool statistics:");
    println!("  • Overall hit ratio: {:.2}%", initial_stats.overall_hit_ratio());
    println!("  • Total allocations: {}", initial_stats.total_allocations());
    
    // Perform memory-intensive operations using pools
    println!("Performing 10,000 buffer operations...");
    
    for i in 0..10_000 {
        // Simulate different buffer usage patterns
        match i % 3 {
            0 => {
                let mut buf = get_small_buffer();
                buf.extend_from_slice(b"small data");
                // Buffer automatically returns to pool on drop
            }
            1 => {
                let mut buf = get_medium_buffer();
                buf.extend_from_slice(&vec![0u8; 2048]);
                // Buffer automatically returns to pool on drop
            }
            _ => {
                let mut buf = get_large_buffer();
                buf.extend_from_slice(&vec![0u8; 8192]);
                // Buffer automatically returns to pool on drop
            }
        }
    }
    
    // Show final pool statistics
    let final_stats = global_pools().combined_stats();
    println!("Final pool statistics:");
    println!("  • Overall hit ratio: {:.2}%", final_stats.overall_hit_ratio());
    println!("  • Total allocations: {}", final_stats.total_allocations());
    println!("  • Cache efficiency improvement: {:.1}x", 
             1.0 / (1.0 - final_stats.overall_hit_ratio() / 100.0));
    
    Ok(())
}

async fn demo_batch_processing() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n📦 Batch Processing Demo");
    println!("------------------------");
    
    let timer_manager = Arc::new(TimerManager::new());
    let config = BatchConfig {
        max_batch_size: 100,
        max_batch_delay_ms: 50,
        channel_capacity: 1000,
    };
    
    let batch_processor = TTLBatchProcessor::new(config, timer_manager);
    let now = current_timestamp();
    
    println!("Scheduling 5,000 messages for batch processing...");
    
    // Schedule messages with short TTL for immediate batch processing
    for i in 0..5_000 {
        let message = Arc::new(WireMessage {
            frame: Bytes::from(format!("batch-message-{}", i)),
            expire_at: now + (i % 100) as u64, // Very short TTL for demo
        });
        
        batch_processor.schedule_message(message).await?;
    }
    
    // Wait for batch processing to complete
    println!("Waiting for batch processing...");
    sleep(Duration::from_millis(200)).await;
    
    let (batch_stats, timer_stats) = batch_processor.stats();
    println!("Batch Processing Statistics:");
    println!("  • Total batches processed: {}", batch_stats.total_batches);
    println!("  • Total items processed:   {}", batch_stats.total_items);
    println!("  • Average batch size:      {:.1}", batch_stats.avg_batch_size);
    println!("  • Max batch size:          {}", batch_stats.max_batch_size);
    println!("  • Batches flushed by size: {}", batch_stats.flush_by_size);
    println!("  • Batches flushed by time: {}", batch_stats.flush_by_timeout);
    
    Ok(())
}

async fn demo_performance_stats() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n📊 Combined Performance Statistics");
    println!("-----------------------------------");
    
    // Show comprehensive statistics from all components
    let pool_stats = global_pools().combined_stats();
    
    println!("Memory Pool Performance:");
    println!("  • Small buffer hit ratio:  {:.2}%", pool_stats.buffer_small.hit_ratio);
    println!("  • Medium buffer hit ratio: {:.2}%", pool_stats.buffer_medium.hit_ratio);
    println!("  • Large buffer hit ratio:  {:.2}%", pool_stats.buffer_large.hit_ratio);
    println!("  • Message batch hit ratio: {:.2}%", pool_stats.message_batch.hit_ratio);
    
    println!("\nPool Utilization:");
    println!("  • Small buffer pool size:  {}", pool_stats.buffer_small.pool_size);
    println!("  • Medium buffer pool size: {}", pool_stats.buffer_medium.pool_size);
    println!("  • Large buffer pool size:  {}", pool_stats.buffer_large.pool_size);
    
    // Performance recommendations
    println!("\n💡 Performance Tips:");
    if pool_stats.overall_hit_ratio() > 90.0 {
        println!("  ✅ Memory pools are performing excellently (>90% hit ratio)");
    } else if pool_stats.overall_hit_ratio() > 70.0 {
        println!("  ⚡ Memory pools are performing well (>70% hit ratio)");
        println!("     Consider warming up pools more aggressively for better performance");
    } else {
        println!("  ⚠️  Memory pool hit ratio is low (<70%)");
        println!("     Consider increasing pool sizes or adjusting usage patterns");
    }
    
    println!("  🔧 For production deployment:");
    println!("     - Enable timer wheel with 10ms resolution");
    println!("     - Use batch processing for TTL cleanup");
    println!("     - Enable CPU affinity for better NUMA performance");
    println!("     - Configure larger memory pools for sustained workloads");
    
    Ok(())
}

fn create_demo_message(id: u64, ttl_ms: u64) -> Arc<WireMessage> {
    Arc::new(WireMessage {
        frame: Bytes::from(format!("demo-message-{}", id)),
        expire_at: current_timestamp() + ttl_ms,
    })
}