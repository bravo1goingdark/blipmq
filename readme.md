<p align="center">
  <img src="./assets/blipmq.jpg" alt="BlipMQ Logo" width="100%" />
</p>

<h1 align="center">🚀 BlipMQ</h1>

<p align="center">
  <b>BlipMQ</b> is an ultra-lightweight, fault-tolerant message queue written in Rust — built for edge, embedded, and developer-first environments.
</p>

<p align="center">
  ⚡ <i>“Kafka-level durability. MQTT-level simplicity. NATS-level performance — all in one binary.”</i>
</p>

---

## 🧩 Features — `v1.0.0`

✅ = Implemented in `v1.0.0`  
⬜ = Planned for future

### 🔌 Core Broker
- ✅ Single static binary (no runtime deps)
- ✅ TCP-based protobuf protocol
- ✅ Topic-based publish/subscribe
- ✅ QoS 0 & QoS 1 support
- ✅ Per-subscriber isolated in-memory queues
- ✅ Configurable TTL and max queue size
- ✅ Overflow policies: `drop_oldest`, `drop_new`, `block`

### 🔐 Durability & Safety
- ✅ Append-only Write-Ahead Log (WAL)
- ✅ WAL segmentation (rotated files)
- ✅ Replay unacknowledged messages on restart
- ✅ CRC32 checksum for corruption detection
- ✅ Batched WAL flushing with fsync

### 📈 Observability
- ✅ Prometheus `/metrics` endpoint
- ✅ Tracing + structured logs
- ✅ Connection + delivery stats

### 🧰 Operational Controls
- ✅ Configurable limits (connections, queue depth)
- ✅ API-key based authentication

---

## 💡 Ideal Use Cases

| Scenario                        | Why BlipMQ?                               |
|---------------------------------|-------------------------------------------|
| 🛰️ IoT or edge gateways          | Single-binary durability, low memory use   |
| 🧪 Local testing/dev environments| Embedded broker with crash recovery        |
| ⚙️ Internal microservice bus      | Fast pub/sub with no external dependencies |
| 🧱 CI/CD pipelines               | Durable test event ingestion               |

---

## 📦 Installation

```bash
curl -LO https://github.com/blipmq/blipmq/releases/download/v1.0.0/blipmq-x86_64-linux
chmod +x blipmq-x86_64-linux
./blipmq-x86_64-linux --config blipmq.toml
