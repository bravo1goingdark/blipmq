use criterion::{criterion_group, criterion_main, Criterion};
use hdrhistogram::Histogram;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn bench_params() -> (usize, usize) {
    let quick = std::env::var("BLIPMQ_QUICK_BENCH")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if quick {
        (8, 20_000) // subs, msgs
    } else {
        (48, 48_000)
    }
}

fn now_micros() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros()
}

fn run_nats_benchmark_inner() -> anyhow::Result<(f64, f64, Histogram<u64>)> {
    let (num_subs, num_msgs) = bench_params();
    let subject = "bench.subject";

    // Connect publisher
    let nc_pub = nats::connect("nats://127.0.0.1:4222")?;

    // Barrier to sync subscribers start
    let barrier = Arc::new(Barrier::new(num_subs + 1));

    // Spawn subscribers
    let mut handles = Vec::with_capacity(num_subs);
    for _ in 0..num_subs {
        let nc = nats::connect("nats://127.0.0.1:4222")?;
        let sub = nc.subscribe(subject)?;
        let barrier = barrier.clone();

        handles.push(thread::spawn(move || -> anyhow::Result<Histogram<u64>> {
            barrier.wait();
            let mut received = 0usize;
            let mut hist = Histogram::<u64>::new(3)?;
            while received < num_msgs {
                match sub.next_timeout(Duration::from_secs(10)) {
                    Ok(msg) => {
                        // payload: "<seq>|<micros>"
                        if let Ok(s) = std::str::from_utf8(&msg.data) {
                            if let Some(ts_str) = s.split('|').nth(1) {
                                if let Ok(sent) = ts_str.parse::<u128>() {
                                    let now = now_micros();
                                    if now > sent { let _ = hist.record((now - sent) as u64); }
                                }
                            }
                        }
                        received += 1;
                    }
                    Err(e) => {
                        anyhow::bail!("Error or timeout waiting for messages: {}", e);
                    }
                }
            }
            Ok(hist)
        }));
    }

    // Give subscribers a moment to register
    std::thread::sleep(Duration::from_millis(200));

    // Start
    barrier.wait();
    let start = Instant::now();

    // Publish messages
    for i in 0..num_msgs {
        let payload = format!("{}|{}", i, now_micros());
        nc_pub.publish(subject, payload)?;
        // Optional: small sleep to simulate burstiness (disabled by default)
        // if i % 10_000 == 0 { std::thread::sleep(Duration::from_millis(1)); }
    }
    nc_pub.flush()?;

    // Aggregate histograms
    let mut merged = Histogram::<u64>::new(3)?;
    for h in handles {
        let sub_hist = h.join().expect("sub thread panicked")?;
        merged.add(&sub_hist)?;
    }

    let elapsed = start.elapsed();
    let total = (num_msgs * num_subs) as f64;
    let thrpt = total / elapsed.as_secs_f64();
    let mean_latency_us = (elapsed.as_secs_f64() / total) * 1e6;

    Ok((thrpt, mean_latency_us, merged))
}

fn nats_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("nats_tcp");
    group.bench_function("nats_fanout", |b| {
        b.iter(|| {
            let (thrpt, mean_lat_us, hist) = run_nats_benchmark_inner().expect("nats bench ok");
            // Print a concise summary so we can grep results from CI logs
            println!(
                "NATS → throughput={:.2} msg/s, mean={:.2}µs, p50={}µs, p95={}µs, p99={}µs",
                thrpt,
                mean_lat_us,
                hist.value_at_quantile(0.5),
                hist.value_at_quantile(0.95),
                hist.value_at_quantile(0.99)
            );
        });
    });
    group.finish();
}

criterion_group!(benches, nats_benchmark);
criterion_main!(benches);

