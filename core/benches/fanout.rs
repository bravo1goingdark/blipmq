use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use corelib::{
    Broker, BrokerConfig, ClientId, DeliveryHandle, PushReceiver, QoSLevel, TopicName,
};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

/// Capacity for each push subscriber's channel in the bench. Big enough to
/// avoid `try_send` failures swamping the measurement, small enough to keep
/// per-bench memory reasonable for the 512-sub case.
const PUSH_CHANNEL_CAPACITY: usize = 65_536;

fn make_broker() -> Broker {
    Broker::new(BrokerConfig {
        default_qos: QoSLevel::AtMostOnce,
        message_ttl: Duration::from_secs(60),
        per_subscriber_queue_capacity: 16_384,
        max_retries: 3,
        retry_base_delay: Duration::from_millis(50),
    })
}

/// Build a broker with `n` v2 push subscribers on `bench/topic`. Each
/// subscriber gets its own bounded mpsc channel (the production shape);
/// receivers are kept alive for the duration of the bench so the broker's
/// `try_send` succeeds — they're held in the returned `Vec<Receiver<_>>`.
fn build_push_broker(
    rt: &tokio::runtime::Runtime,
    num_subs: usize,
    qos: QoSLevel,
) -> (Arc<Broker>, TopicName, Vec<PushReceiver>) {
    let broker = Arc::new(make_broker());
    let topic = TopicName::new("bench/topic");
    let mut receivers = Vec::with_capacity(num_subs);
    let _g = rt.enter();
    for i in 0..num_subs {
        let (tx, rx) = flume::bounded::<DeliveryHandle>(PUSH_CHANNEL_CAPACITY);
        let conn_id = (i + 1) as u64;
        broker.subscribe_with_conn(
            ClientId::new(format!("c{i}")),
            topic.clone(),
            qos,
            conn_id,
            tx,
        );
        receivers.push(rx);
    }
    (broker, topic, receivers)
}

/// Push-path fanout: this is the bench that mirrors the production v2 hot
/// path (`publish_with_wal_id` -> per-sub `try_send`). With each iteration
/// the publisher pushes one message; for fanout=N the broker dispatches N
/// `DeliveryHandle`s. We do NOT drain the receivers inside the timed
/// section — the goal is to measure broker fanout cost, not channel-recv
/// throughput.
fn bench_publish_push_qos0(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let mut group = c.benchmark_group("publish_qos0_push_fanout");
    let payload = Bytes::from(vec![0u8; 256]);

    for &subs in &[1usize, 8, 64, 512] {
        group.throughput(Throughput::Elements(subs as u64));
        group.bench_with_input(BenchmarkId::from_parameter(subs), &subs, |b, &subs| {
            let (broker, topic, receivers) = build_push_broker(&rt, subs, QoSLevel::AtMostOnce);
            // Drain in a background task so the per-sub channels never fill.
            // `bench_with_input` re-builds state per iteration sample, so
            // these tasks are short-lived.
            for rx in receivers {
                rt.spawn(async move {
                    while rx.recv_async().await.is_ok() {}
                });
            }
            b.iter(|| {
                broker.publish(&topic, payload.clone(), QoSLevel::AtMostOnce);
            });
        });
    }

    group.finish();
}

fn bench_publish_push_qos1(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let mut group = c.benchmark_group("publish_qos1_push_fanout");
    let payload = Bytes::from(vec![0u8; 256]);

    for &subs in &[1usize, 8, 64, 512] {
        group.throughput(Throughput::Elements(subs as u64));
        group.bench_with_input(BenchmarkId::from_parameter(subs), &subs, |b, &subs| {
            let (broker, topic, receivers) = build_push_broker(&rt, subs, QoSLevel::AtLeastOnce);
            for rx in receivers {
                rt.spawn(async move {
                    while rx.recv_async().await.is_ok() {}
                });
            }
            b.iter(|| {
                broker.publish(&topic, payload.clone(), QoSLevel::AtLeastOnce);
            });
        });
    }

    group.finish();
}

/// Legacy poll-path fanout — kept as a regression guard so we notice if the
/// poll path ever degrades, but it is no longer the headline metric.
fn bench_publish_poll_qos0(c: &mut Criterion) {
    let mut group = c.benchmark_group("publish_qos0_poll_fanout");
    let payload = Bytes::from(vec![0u8; 256]);

    for &subs in &[1usize, 8, 64, 512] {
        group.throughput(Throughput::Elements(subs as u64));
        group.bench_with_input(BenchmarkId::from_parameter(subs), &subs, |b, &subs| {
            let broker = make_broker();
            let topic = TopicName::new("bench/topic");
            for i in 0..subs {
                broker.subscribe(ClientId::new(format!("c{i}")), topic.clone(), QoSLevel::AtMostOnce);
            }
            b.iter(|| {
                broker.publish(&topic, payload.clone(), QoSLevel::AtMostOnce);
            });
        });
    }

    group.finish();
}

fn bench_publish_payload_sizes(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let mut group = c.benchmark_group("publish_payload_sizes_push_64subs");
    let (broker, topic, receivers) = build_push_broker(&rt, 64, QoSLevel::AtMostOnce);
    for rx in receivers {
        rt.spawn(async move {
            while rx.recv_async().await.is_ok() {}
        });
    }

    for &size in &[64usize, 1024, 16 * 1024] {
        let payload = Bytes::from(vec![0u8; size]);
        group.throughput(Throughput::Bytes((size * 64) as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| {
                broker.publish(&topic, payload.clone(), QoSLevel::AtMostOnce);
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_publish_push_qos0,
    bench_publish_push_qos1,
    bench_publish_poll_qos0,
    bench_publish_payload_sizes
);
criterion_main!(benches);
