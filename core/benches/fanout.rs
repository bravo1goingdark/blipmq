use std::time::Duration;

use bytes::Bytes;
use corelib::{Broker, BrokerConfig, ClientId, QoSLevel, TopicName};
use criterion::{
    criterion_group, criterion_main, BenchmarkId, Criterion, Throughput,
};

fn make_broker() -> Broker {
    Broker::new(BrokerConfig {
        default_qos: QoSLevel::AtMostOnce,
        message_ttl: Duration::from_secs(60),
        per_subscriber_queue_capacity: 16_384,
        max_retries: 3,
        retry_base_delay: Duration::from_millis(50),
    })
}

fn build_subscribed_broker(num_subscribers: usize, qos: QoSLevel) -> (Broker, TopicName) {
    let broker = make_broker();
    let topic = TopicName::new("bench/topic");
    for i in 0..num_subscribers {
        broker.subscribe(ClientId::new(format!("c{i}")), topic.clone(), qos);
    }
    (broker, topic)
}

fn bench_publish_qos0(c: &mut Criterion) {
    let mut group = c.benchmark_group("publish_qos0_fanout");
    let payload = Bytes::from(vec![0u8; 256]);

    for &subs in &[1usize, 8, 64, 512] {
        group.throughput(Throughput::Elements(subs as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(subs),
            &subs,
            |b, &subs| {
                let (broker, topic) = build_subscribed_broker(subs, QoSLevel::AtMostOnce);
                b.iter(|| {
                    broker.publish(&topic, payload.clone(), QoSLevel::AtMostOnce);
                });
            },
        );
    }

    group.finish();
}

fn bench_publish_qos1(c: &mut Criterion) {
    let mut group = c.benchmark_group("publish_qos1_fanout");
    let payload = Bytes::from(vec![0u8; 256]);

    for &subs in &[1usize, 8, 64, 512] {
        group.throughput(Throughput::Elements(subs as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(subs),
            &subs,
            |b, &subs| {
                let (broker, topic) = build_subscribed_broker(subs, QoSLevel::AtLeastOnce);
                b.iter(|| {
                    broker.publish(&topic, payload.clone(), QoSLevel::AtLeastOnce);
                });
            },
        );
    }

    group.finish();
}

fn bench_publish_payload_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("publish_payload_sizes_qos0_64subs");
    let (broker, topic) = build_subscribed_broker(64, QoSLevel::AtMostOnce);

    for &size in &[64usize, 1024, 16 * 1024] {
        let payload = Bytes::from(vec![0u8; size]);
        group.throughput(Throughput::Bytes((size * 64) as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(size),
            &size,
            |b, _| {
                b.iter(|| {
                    broker.publish(&topic, payload.clone(), QoSLevel::AtMostOnce);
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_publish_qos0,
    bench_publish_qos1,
    bench_publish_payload_sizes
);
criterion_main!(benches);
