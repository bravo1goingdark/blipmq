# 🚀 BlipMQ — Lightweight, Durable, Single-Binary Message Broker

**BlipMQ** is an ultra-lightweight, fault-tolerant message queue written in Rust — designed for local-first, edge, and high-performance embedded systems. It offers **durability**, **per-subscriber isolation**, and **QoS-aware delivery**, all in a **single deployable binary**.

> Think: MQTT simplicity meets Kafka reliability — without the complexity.

---

## 🧩 Features — v1.0.0

✅ = Included in `v1.0.0`

### Core Functionality
- [x] Single-node deployment (no external services required)
- [x] TCP-based binary protocol (Protobuf framed)
- [x] Publish / Subscribe with topic-based routing
- [x] Per-subscriber isolated in-memory queues
- [x] QoS 0 (fire-and-forget) and QoS 1 (retry until ack)
- [x] Configurable TTL for message expiration
- [x] Configurable queue depth with drop policies:
    - [x] Drop oldest
    - [x] Drop new
    - [x] Block publisher (backpressure)
- [x] Unique message IDs per publish

### Durability & Recovery
- [x] Append-only Write-Ahead Log (WAL) for durability
- [x] WAL-backed crash recovery (replay unacked messages)
- [x] WAL segmentation (fixed-size rotated files)
- [x] CRC32 checksums per WAL record
- [x] Batched WAL flush with optional fsync

### Performance & Observability
- [x] High-throughput async architecture (powered by `tokio`)
- [x] Prometheus-compatible `/metrics` HTTP endpoint
- [x] Tracing and logging with structured events

### Security & Access
- [x] Static API key-based authentication per client
- [x] Connection limits and rate limiting (configurable)

---

## 🚦 Ideal Use Cases

- Local-first apps needing embedded queues
- Edge devices or gateways (e.g. IoT ingestion)
- CI/CD pipelines that need a drop-in pub/sub layer
- Durable telemetry collectors in constrained environments
- Lightweight alternatives to Kafka, RabbitMQ, NATS

---

