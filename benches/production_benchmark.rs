use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use hdrhistogram::Histogram;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;

struct CountingAllocator;

static GLOBAL_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        GLOBAL_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        GLOBAL_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        System.alloc_zeroed(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        GLOBAL_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

const WARMUP_MESSAGES: usize = 10_000;
const PAYLOAD_SIZES: &[usize] = &[64, 256, 1024, 4096, 16384];
const SUBSCRIBER_COUNTS: &[usize] = &[1, 10, 50, 100, 500];

/// Production benchmark configuration
struct BenchConfig {
    pub messages: usize,
    pub payload_size: usize,
    pub subscribers: usize,
    pub publishers: usize,
    pub topics: usize,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            messages: 1_000_000,
            payload_size: 256,
            subscribers: 100,
            publishers: 10,
            topics: 10,
        }
    }
}

/// Benchmark results with detailed statistics
#[derive(Debug, Clone)]
struct BenchmarkResult {
    pub throughput_ops: f64,
    pub throughput_mbps: f64,
    pub latency_p50_us: u64,
    pub latency_p95_us: u64,
    pub latency_p99_us: u64,
    pub latency_p999_us: u64,
    pub latency_max_us: u64,
    pub cpu_usage: f64,
    pub memory_mb: f64,
}

/// Run BlipMQ benchmark
async fn bench_blipmq(config: &BenchConfig) -> BenchmarkResult {
    use blipmq::start_broker;
    use tokio::net::TcpStream;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use bytes::{BytesMut, BufMut};
    
    // Start broker
    let broker_handle = tokio::spawn(async move {
        let _ = start_broker().await;
    });
    
    tokio::time::sleep(Duration::from_millis(100)).await;
    
    let payload = vec![0xAB; config.payload_size];
    let mut latency_hist = Histogram::<u64>::new(3).unwrap();
    let messages_sent = Arc::new(AtomicUsize::new(0));
    let bytes_sent = Arc::new(AtomicU64::new(0));
    
    // Create subscribers
    let mut sub_handles = Vec::new();
    for sub_id in 0..config.subscribers {
        let messages_to_receive = config.messages / config.subscribers;
        let hist = Arc::new(tokio::sync::Mutex::new(Histogram::<u64>::new(3).unwrap()));
        
        let handle = tokio::spawn(async move {
            let mut stream = TcpStream::connect("127.0.0.1:8080").await.unwrap();
            stream.set_nodelay(true).unwrap();
            
            // Subscribe using new protocol
            let topic = format!("bench/topic/{}", sub_id % config.topics);
            let mut frame = BytesMut::new();
            frame.put_u32(8 + topic.len() as u32); // frame_size
            frame.put_u8(0x20); // OpCode::Sub << 4
            frame.put_u8(topic.len() as u8);
            frame.put_u16(0); // reserved
            frame.put_slice(topic.as_bytes());
            
            stream.write_all(&frame).await.unwrap();
            
            // Read acknowledgment
            let mut ack_buf = vec![0u8; 256];
            let _ = stream.read(&mut ack_buf).await.unwrap();
            
            // Receive messages
            let mut received = 0;
            let mut buf = vec![0u8; 65536];
            while received < messages_to_receive {
                match stream.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        // Parse timestamp from payload for latency measurement
                        if n >= 16 {
                            let timestamp = u64::from_le_bytes([
                                buf[n-8], buf[n-7], buf[n-6], buf[n-5],
                                buf[n-4], buf[n-3], buf[n-2], buf[n-1],
                            ]);
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_micros() as u64;
                            
                            if now > timestamp {
                                let latency = now - timestamp;
                                let mut h = hist.lock().await;
                                let _ = h.record(latency);
                            }
                        }
                        received += 1;
                    }
                    Err(_) => break,
                }
            }
            
            hist
        });
        
        sub_handles.push(handle);
    }
    
    // Wait for subscribers to connect
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    // Warmup phase
    for _ in 0..WARMUP_MESSAGES {
        let mut stream = TcpStream::connect("127.0.0.1:8080").await.unwrap();
        stream.set_nodelay(true).unwrap();
        
        let topic = format!("bench/topic/{}", 0);
        let mut frame = BytesMut::new();
        frame.put_u32(8 + topic.len() as u32 + payload.len() as u32 + 12);
        frame.put_u8(0x10); // OpCode::Pub << 4
        frame.put_u8(topic.len() as u8);
        frame.put_u16(0);
        frame.put_slice(topic.as_bytes());
        frame.put_u32(0); // TTL
        frame.put_slice(&payload);
        
        let _ = stream.write_all(&frame).await;
    }
    
    // Benchmark phase
    let start = Instant::now();
    let mut pub_handles = Vec::new();
    
    for pub_id in 0..config.publishers {
        let messages_per_pub = config.messages / config.publishers;
        let payload = payload.clone();
        let messages_sent = messages_sent.clone();
        let bytes_sent = bytes_sent.clone();
        let topics = config.topics;
        
        let handle = tokio::spawn(async move {
            let mut stream = TcpStream::connect("127.0.0.1:8080").await.unwrap();
            stream.set_nodelay(true).unwrap();
            
            for i in 0..messages_per_pub {
                let topic = format!("bench/topic/{}", i % topics);
                let mut frame = BytesMut::new();
                
                // Add timestamp to payload for latency measurement
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_micros() as u64;
                
                frame.put_u32(8 + topic.len() as u32 + payload.len() as u32 + 12);
                frame.put_u8(0x10); // OpCode::Pub << 4
                frame.put_u8(topic.len() as u8);
                frame.put_u16(0);
                frame.put_slice(topic.as_bytes());
                frame.put_u32(0); // TTL
                frame.put_slice(&payload);
                frame.put_u64_le(timestamp);
                
                stream.write_all(&frame).await.unwrap();
                
                messages_sent.fetch_add(1, Ordering::Relaxed);
                bytes_sent.fetch_add(frame.len() as u64, Ordering::Relaxed);
            }
        });
        
        pub_handles.push(handle);
    }
    
    // Wait for publishers
    for handle in pub_handles {
        handle.await.unwrap();
    }
    
    let duration = start.elapsed();
    
    // Collect latency histograms
    for handle in sub_handles {
        if let Ok(hist) = handle.await {
            let h = hist.lock().await;
            latency_hist.add(&*h).unwrap();
        }
    }
    
    // Calculate metrics
    let total_messages = messages_sent.load(Ordering::Relaxed);
    let total_bytes = bytes_sent.load(Ordering::Relaxed);
    let throughput_ops = total_messages as f64 / duration.as_secs_f64();
    let throughput_mbps = (total_bytes as f64 / duration.as_secs_f64()) / (1024.0 * 1024.0);
    
    broker_handle.abort();
    
    BenchmarkResult {
        throughput_ops,
        throughput_mbps,
        latency_p50_us: latency_hist.value_at_quantile(0.5),
        latency_p95_us: latency_hist.value_at_quantile(0.95),
        latency_p99_us: latency_hist.value_at_quantile(0.99),
        latency_p999_us: latency_hist.value_at_quantile(0.999),
        latency_max_us: latency_hist.max(),
        cpu_usage: 0.0, // TODO: Implement CPU measurement
        memory_mb: 0.0,  // TODO: Implement memory measurement
    }
}

/// Run NATS benchmark for comparison
async fn bench_nats(config: &BenchConfig) -> BenchmarkResult {
    use nats;
    
    // Start embedded NATS server or connect to existing
    let nc = nats::connect("nats://localhost:4222").unwrap();
    
    let payload = vec![0xAB; config.payload_size];
    let mut latency_hist = Histogram::<u64>::new(3).unwrap();
    let messages_sent = Arc::new(AtomicUsize::new(0));
    let bytes_sent = Arc::new(AtomicU64::new(0));
    
    // Create subscribers
    let mut subs = Vec::new();
    for sub_id in 0..config.subscribers {
        let topic = format!("bench.topic.{}", sub_id % config.topics);
        let sub = nc.subscribe(&topic).unwrap();
        subs.push(sub);
    }
    
    // Warmup
    for _ in 0..WARMUP_MESSAGES {
        nc.publish("bench.topic.0", &payload).unwrap();
    }
    nc.flush().unwrap();
    
    // Benchmark phase
    let start = Instant::now();
    
    for i in 0..config.messages {
        let topic = format!("bench.topic.{}", i % config.topics);
        
        // Add timestamp for latency
        let mut msg = payload.clone();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;
        msg.extend_from_slice(&timestamp.to_le_bytes());
        
        nc.publish(&topic, &msg).unwrap();
        
        messages_sent.fetch_add(1, Ordering::Relaxed);
        bytes_sent.fetch_add(msg.len() as u64, Ordering::Relaxed);
        
        // Flush periodically for better performance
        if i % 1000 == 0 {
            nc.flush().unwrap();
        }
    }
    
    nc.flush().unwrap();
    let duration = start.elapsed();
    
    // Process received messages for latency
    for sub in subs {
        while let Ok(msg) = sub.try_next() {
            if msg.data.len() >= 8 {
                let timestamp = u64::from_le_bytes([
                    msg.data[msg.data.len()-8], msg.data[msg.data.len()-7],
                    msg.data[msg.data.len()-6], msg.data[msg.data.len()-5],
                    msg.data[msg.data.len()-4], msg.data[msg.data.len()-3],
                    msg.data[msg.data.len()-2], msg.data[msg.data.len()-1],
                ]);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_micros() as u64;
                
                if now > timestamp {
                    let _ = latency_hist.record(now - timestamp);
                }
            }
        }
    }
    
    let total_messages = messages_sent.load(Ordering::Relaxed);
    let total_bytes = bytes_sent.load(Ordering::Relaxed);
    
    BenchmarkResult {
        throughput_ops: total_messages as f64 / duration.as_secs_f64(),
        throughput_mbps: (total_bytes as f64 / duration.as_secs_f64()) / (1024.0 * 1024.0),
        latency_p50_us: latency_hist.value_at_quantile(0.5),
        latency_p95_us: latency_hist.value_at_quantile(0.95),
        latency_p99_us: latency_hist.value_at_quantile(0.99),
        latency_p999_us: latency_hist.value_at_quantile(0.999),
        latency_max_us: latency_hist.max(),
        cpu_usage: 0.0,
        memory_mb: 0.0,
    }
}

fn benchmark_latency(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("latency");
    
    for &payload_size in &[64, 256, 1024] {
        let config = BenchConfig {
            messages: 100_000,
            payload_size,
            subscribers: 10,
            publishers: 1,
            topics: 1,
        };
        
        group.throughput(Throughput::Bytes(payload_size as u64));
        
        group.bench_with_input(
            BenchmarkId::new("BlipMQ", payload_size),
            &config,
            |b, config| {
                b.to_async(&rt).iter(|| async {
                    bench_blipmq(config).await
                });
            },
        );
        
        // Uncomment to compare with NATS
        // group.bench_with_input(
        //     BenchmarkId::new("NATS", payload_size),
        //     &config,
        //     |b, config| {
        //         b.to_async(&rt).iter(|| async {
        //             bench_nats(config).await
        //         });
        //     },
        // );
    }
    
    group.finish();
}

fn benchmark_throughput(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("throughput");
    
    for &subscribers in &[10, 50, 100] {
        let config = BenchConfig {
            messages: 1_000_000,
            payload_size: 256,
            subscribers,
            publishers: 10,
            topics: 10,
        };
        
        group.throughput(Throughput::Elements(config.messages as u64));
        
        group.bench_with_input(
            BenchmarkId::new("BlipMQ", subscribers),
            &config,
            |b, config| {
                b.to_async(&rt).iter(|| async {
                    bench_blipmq(config).await
                });
            },
        );
    }
    
    group.finish();
}

fn benchmark_fanout(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("fanout");

    for &subscribers in SUBSCRIBER_COUNTS {
        let config = BenchConfig {
            messages: 100_000,
            payload_size: 256,
            subscribers,
            publishers: 1,
            topics: 1,
        };
        
        let total_messages = config.messages * subscribers;
        group.throughput(Throughput::Elements(total_messages as u64));
        
        group.bench_with_input(
            BenchmarkId::new("BlipMQ", subscribers),
            &config,
            |b, config| {
                b.to_async(&rt).iter(|| async {
                    bench_blipmq(config).await
                });
            },
        );
    }
    
    group.finish();
}

fn benchmark_encoder(c: &mut Criterion) {
    use blipmq::core::message::{encode_message_frame, Message};
    use bytes::Bytes;

    let mut group = c.benchmark_group("message_encoder");

    for &payload_size in PAYLOAD_SIZES {
        group.throughput(Throughput::Elements(1));

        group.bench_with_input(
            BenchmarkId::new("encode_frame", payload_size),
            &payload_size,
            |b, &size| {
                let payload = Bytes::from(vec![0u8; size]);
                let mut message = Message {
                    id: 1,
                    payload,
                    timestamp: 0,
                    ttl_ms: 0,
                };

                black_box(encode_message_frame(&message));

                b.iter_custom(|iters| {
                    let start_allocs = GLOBAL_ALLOCATIONS.load(Ordering::Relaxed);
                    let start = Instant::now();
                    let mut id = message.id;
                    for _ in 0..iters {
                        id = id.wrapping_add(1);
                        message.id = id;
                        let frame = encode_message_frame(&message);
                        black_box(frame);
                    }
                    let duration = start.elapsed();
                    let end_allocs = GLOBAL_ALLOCATIONS.load(Ordering::Relaxed);
                    let allocations = end_allocs - start_allocs;
                    assert!(
                        allocations == 0,
                        "encoder incurred {} allocations for payload {} bytes",
                        allocations,
                        size
                    );
                    duration
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    benchmark_latency,
    benchmark_throughput,
    benchmark_fanout,
    benchmark_encoder
);
criterion_main!(benches);
