//! Performance test client for BlipMQ

use std::net::TcpStream;
use std::io::{Read, Write};
use std::thread;
use std::time::{Duration, Instant};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

fn main() {
    println!("BlipMQ Performance Test Client");
    println!("==============================");
    
    let num_publishers = 10;
    let num_subscribers = 10;
    let messages_per_publisher = 10000;
    
    let total_messages = Arc::new(AtomicU64::new(0));
    let total_bytes = Arc::new(AtomicU64::new(0));
    
    let start = Instant::now();
    
    // Spawn subscribers
    let mut sub_handles = Vec::new();
    for i in 0..num_subscribers {
        let total_messages = Arc::clone(&total_messages);
        let total_bytes = Arc::clone(&total_bytes);
        
        let handle = thread::spawn(move || {
            if let Ok(mut stream) = TcpStream::connect("127.0.0.1:5555") {
                // Subscribe to topic
                let topic = format!("test{}", i % 5);
                let subscribe_msg = format!("S{}{}", topic.len() as u8 as char, topic);
                stream.write_all(subscribe_msg.as_bytes()).ok();
                
                println!("Subscriber {} connected and subscribed to {}", i, topic);
                
                // Read messages
                let mut buffer = [0u8; 65536];
                let mut count = 0;
                
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            count += 1;
                            total_messages.fetch_add(1, Ordering::Relaxed);
                            total_bytes.fetch_add(n as u64, Ordering::Relaxed);
                            
                            if count % 1000 == 0 {
                                println!("Subscriber {} received {} messages", i, count);
                            }
                        }
                        Err(_) => break,
                    }
                }
                
                println!("Subscriber {} disconnected after {} messages", i, count);
            }
        });
        
        sub_handles.push(handle);
    }
    
    // Give subscribers time to connect
    thread::sleep(Duration::from_millis(100));
    
    // Spawn publishers
    let mut pub_handles = Vec::new();
    for i in 0..num_publishers {
        let handle = thread::spawn(move || {
            if let Ok(mut stream) = TcpStream::connect("127.0.0.1:5555") {
                println!("Publisher {} connected", i);
                
                for j in 0..messages_per_publisher {
                    let topic = format!("test{}", j % 5);
                    let payload = format!("Message {} from publisher {}", j, i);
                    let publish_msg = format!("P{}{}{}", 
                        topic.len() as u8 as char, 
                        topic, 
                        payload
                    );
                    
                    if stream.write_all(publish_msg.as_bytes()).is_err() {
                        break;
                    }
                    
                    if j % 1000 == 0 {
                        println!("Publisher {} sent {} messages", i, j);
                    }
                }
                
                println!("Publisher {} finished", i);
            }
        });
        
        pub_handles.push(handle);
    }
    
    // Wait for publishers to finish
    for handle in pub_handles {
        handle.join().ok();
    }
    
    // Wait a bit for messages to be delivered
    thread::sleep(Duration::from_secs(2));
    
    let elapsed = start.elapsed();
    let total_msgs = total_messages.load(Ordering::Relaxed);
    let total_bts = total_bytes.load(Ordering::Relaxed);
    
    println!("\n=== Performance Results ===");
    println!("Time: {:.2}s", elapsed.as_secs_f64());
    println!("Messages received: {}", total_msgs);
    println!("Bytes received: {}", total_bts);
    println!("Throughput: {:.0} msg/s", total_msgs as f64 / elapsed.as_secs_f64());
    println!("Bandwidth: {:.2} MB/s", total_bts as f64 / elapsed.as_secs_f64() / 1_000_000.0);
    
    // Cleanup
    println!("\nPress Ctrl+C to stop subscribers...");
    thread::sleep(Duration::from_secs(60));
}
