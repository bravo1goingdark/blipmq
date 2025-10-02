use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, AtomicU64, Ordering};

// Quick or full benchmark
fn bench_params() -> (usize, usize) {
    if std::env::var("QUICK_BENCH").is_ok() {
        (4, 10_000)  // 4 subscribers, 10K messages
    } else {
        (10, 100_000)  // 10 subscribers, 100K messages
    }
}

fn timestamp_micros() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as u64
}

async fn bench_nats() -> anyhow::Result<()> {
    println!("\n📊 NATS Benchmark (Real TCP connections)");
    println!("=========================================");
    
    let (num_subs, num_msgs) = bench_params();
    println!("Configuration: {} subscribers, {} messages", num_subs, num_msgs);
    
    // Connect to NATS
    let nc = nats::connect("nats://127.0.0.1:4222")?;
    
    // Statistics
    let messages_received = Arc::new(AtomicUsize::new(0));
    let total_latency = Arc::new(AtomicU64::new(0));
    let min_latency = Arc::new(AtomicU64::new(u64::MAX));
    let max_latency = Arc::new(AtomicU64::new(0));
    
    // Create subscribers
    let mut subs = Vec::new();
    for i in 0..num_subs {
        let subject = format!("bench.topic.{}", i);
        let sub = nc.subscribe(&subject)?;
        
        let messages_received = messages_received.clone();
        let total_latency = total_latency.clone();
        let min_latency = min_latency.clone();
        let max_latency = max_latency.clone();
        
        // Spawn async task to process messages
        tokio::spawn(async move {
            let mut count = 0;
            while count < num_msgs {
                if let Ok(msg) = sub.next_timeout(Duration::from_secs(30)) {
                    // Parse timestamp from payload
                    if msg.data.len() >= 8 {
                        let ts_bytes: [u8; 8] = msg.data[msg.data.len()-8..].try_into().unwrap_or([0; 8]);
                        let sent_time = u64::from_le_bytes(ts_bytes);
                        let now = timestamp_micros();
                        
                        if now > sent_time {
                            let latency = now - sent_time;
                            total_latency.fetch_add(latency, Ordering::Relaxed);
                            
                            // Update min/max
                            min_latency.fetch_min(latency, Ordering::Relaxed);
                            max_latency.fetch_max(latency, Ordering::Relaxed);
                        }
                    }
                    
                    messages_received.fetch_add(1, Ordering::Relaxed);
                    count += 1;
                }
            }
        });
        
        subs.push(sub);
    }
    
    // Wait for subscribers to be ready
    tokio::time::sleep(Duration::from_millis(100)).await;
    
    // Benchmark publishing
    let payload = vec![0xAB; 256]; // 256 byte payload
    let start = Instant::now();
    
    for msg_num in 0..num_msgs {
        for sub_num in 0..num_subs {
            let subject = format!("bench.topic.{}", sub_num);
            
            // Add timestamp to payload
            let mut msg_data = payload.clone();
            msg_data.extend_from_slice(&timestamp_micros().to_le_bytes());
            
            nc.publish(&subject, msg_data)?;
        }
        
        // Flush every 1000 messages for better throughput
        if msg_num % 1000 == 0 {
            nc.flush()?;
        }
    }
    nc.flush()?;
    
    let publish_time = start.elapsed();
    
    // Wait for all messages to be received
    let wait_start = Instant::now();
    while messages_received.load(Ordering::Relaxed) < (num_msgs * num_subs) {
        if wait_start.elapsed() > Duration::from_secs(30) {
            println!("⚠️  Timeout waiting for messages");
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    
    let total_time = start.elapsed();
    
    // Calculate statistics
    let total_messages = num_msgs * num_subs;
    let received = messages_received.load(Ordering::Relaxed);
    let throughput = received as f64 / total_time.as_secs_f64();
    let avg_latency = if received > 0 {
        total_latency.load(Ordering::Relaxed) as f64 / received as f64
    } else {
        0.0
    };
    
    println!("\n📈 NATS Results:");
    println!("  Total messages: {}", total_messages);
    println!("  Messages received: {}", received);
    println!("  Publish time: {:.2?}", publish_time);
    println!("  Total time: {:.2?}", total_time);
    println!("  Throughput: {:.0} msg/s", throughput);
    println!("  Average latency: {:.0} µs", avg_latency);
    println!("  Min latency: {} µs", min_latency.load(Ordering::Relaxed));
    println!("  Max latency: {} µs", max_latency.load(Ordering::Relaxed));
    
    Ok(())
}

async fn bench_blipmq() -> anyhow::Result<()> {
    use tokio::net::TcpStream;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    
    println!("\n📊 BlipMQ Benchmark (Real TCP connections)");
    println!("===========================================");
    
    let (num_subs, num_msgs) = bench_params();
    println!("Configuration: {} subscribers, {} messages", num_subs, num_msgs);
    
    // Start BlipMQ broker in background
    let broker_handle = tokio::spawn(async {
        let _ = blipmq::start_broker().await;
    });
    
    // Wait for broker to start
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    // Statistics
    let messages_received = Arc::new(AtomicUsize::new(0));
    let total_latency = Arc::new(AtomicU64::new(0));
    let min_latency = Arc::new(AtomicU64::new(u64::MAX));
    let max_latency = Arc::new(AtomicU64::new(0));
    
    // Create subscriber connections
    let mut sub_handles = Vec::new();
    for i in 0..num_subs {
        let messages_received = messages_received.clone();
        let total_latency = total_latency.clone();
        let min_latency = min_latency.clone();
        let max_latency = max_latency.clone();
        
        let handle = tokio::spawn(async move {
            let mut stream = TcpStream::connect("127.0.0.1:8080").await?;
            stream.set_nodelay(true)?;
            
            // Subscribe using FlatBuffers protocol
            let topic = format!("bench/topic/{}", i);
            let cmd = blipmq::core::command::new_sub(&topic);
            let encoded = blipmq::core::command::encode_command(&cmd);
            
            stream.write_all(&(encoded.len() as u32).to_be_bytes()).await?;
            stream.write_all(&encoded).await?;
            stream.flush().await?;
            
            // Read messages
            let mut buf = vec![0u8; 65536];
            let mut received = 0;
            
            while received < num_msgs {
                // Read length prefix
                let mut len_buf = [0u8; 4];
                match stream.read_exact(&mut len_buf).await {
                    Ok(_) => {},
                    Err(_) => break,
                }
                let msg_len = u32::from_be_bytes(len_buf) as usize;
                
                if msg_len > buf.len() {
                    buf.resize(msg_len, 0);
                }
                
                // Read message
                match stream.read_exact(&mut buf[..msg_len]).await {
                    Ok(_) => {
                        // Extract timestamp from end of payload
                        if msg_len >= 8 {
                            let ts_bytes: [u8; 8] = buf[msg_len-8..msg_len].try_into().unwrap_or([0; 8]);
                            let sent_time = u64::from_le_bytes(ts_bytes);
                            let now = timestamp_micros();
                            
                            if now > sent_time {
                                let latency = now - sent_time;
                                total_latency.fetch_add(latency, Ordering::Relaxed);
                                min_latency.fetch_min(latency, Ordering::Relaxed);
                                max_latency.fetch_max(latency, Ordering::Relaxed);
                            }
                        }
                        
                        messages_received.fetch_add(1, Ordering::Relaxed);
                        received += 1;
                    }
                    Err(_) => break,
                }
            }
            
            Ok::<(), anyhow::Error>(())
        });
        
        sub_handles.push(handle);
    }
    
    // Wait for subscribers to connect
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    // Benchmark publishing
    let payload = vec![0xAB; 256]; // 256 byte payload
    let start = Instant::now();
    
    // Create publisher connection
    let mut pub_stream = TcpStream::connect("127.0.0.1:8080").await?;
    pub_stream.set_nodelay(true)?;
    
    for msg_num in 0..num_msgs {
        for sub_num in 0..num_subs {
            let topic = format!("bench/topic/{}", sub_num);
            
            // Add timestamp to payload
            let mut msg_data = payload.clone();
            msg_data.extend_from_slice(&timestamp_micros().to_le_bytes());
            
            let cmd = blipmq::core::command::new_pub(&topic, msg_data);
            let encoded = blipmq::core::command::encode_command(&cmd);
            
            pub_stream.write_all(&(encoded.len() as u32).to_be_bytes()).await?;
            pub_stream.write_all(&encoded).await?;
        }
        
        // Flush periodically
        if msg_num % 100 == 0 {
            pub_stream.flush().await?;
        }
    }
    pub_stream.flush().await?;
    
    let publish_time = start.elapsed();
    
    // Wait for all messages to be received
    let wait_start = Instant::now();
    while messages_received.load(Ordering::Relaxed) < (num_msgs * num_subs) {
        if wait_start.elapsed() > Duration::from_secs(30) {
            println!("⚠️  Timeout waiting for messages");
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    
    let total_time = start.elapsed();
    
    // Calculate statistics
    let total_messages = num_msgs * num_subs;
    let received = messages_received.load(Ordering::Relaxed);
    let throughput = received as f64 / total_time.as_secs_f64();
    let avg_latency = if received > 0 {
        total_latency.load(Ordering::Relaxed) as f64 / received as f64
    } else {
        0.0
    };
    
    println!("\n📈 BlipMQ Results:");
    println!("  Total messages: {}", total_messages);
    println!("  Messages received: {}", received);
    println!("  Publish time: {:.2?}", publish_time);
    println!("  Total time: {:.2?}", total_time);
    println!("  Throughput: {:.0} msg/s", throughput);
    println!("  Average latency: {:.0} µs", avg_latency);
    println!("  Min latency: {} µs", min_latency.load(Ordering::Relaxed));
    println!("  Max latency: {} µs", max_latency.load(Ordering::Relaxed));
    
    // Cleanup
    broker_handle.abort();
    
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("🚀 Real-World TCP Benchmark Suite");
    println!("==================================");
    println!("Comparing BlipMQ vs NATS with actual TCP connections\n");
    
    // Run NATS benchmark
    if let Err(e) = bench_nats().await {
        println!("❌ NATS benchmark failed: {}", e);
    }
    
    println!("\n---\n");
    
    // Run BlipMQ benchmark
    if let Err(e) = bench_blipmq().await {
        println!("❌ BlipMQ benchmark failed: {}", e);
    }
    
    println!("\n✅ Benchmark complete!");
    
    Ok(())
}
