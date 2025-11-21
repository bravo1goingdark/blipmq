# BlipMQ Architecture

BlipMQ is built as a set of focused crates that compose into a high-performance, durable message broker. This document explains the components, how they fit together, and the end-to-end dataflow for common operations.

## Components

At a high level, BlipMQ is structured as:

```text
+-----------+     TCP (HELLO/AUTH/PUB/SUB/POLL/ACK)     +----------------+
| Clients   | <---------------------------------------> |  blipmq_net    |
+-----------+                                          +----------------+
                                                           |
                                                           | BrokerHandler
                                                           v
                                                     +----------------+
                                                     |  blipmq_core   |
                                                     |  (Broker)      |
                                                     +----------------+
                                                           |
                                                           | durable QoS1 publish
                                                           v
                                                     +----------------+
                                                     |  blipmq_wal    |
                                                     +----------------+

                       +----------------+       +------------------+
                       | blipmq_metrics | <---- |    Broker+WAL    |
                       +----------------+       +------------------+

                       +----------------+
                       | blipmq_config |
                       +----------------+

                       +----------------+
                       |  blipmqd       |
                       +----------------+
```

### `blipmq_net` (Networking & Protocol)

- TCP server (`Server`) listening on a configured address.
- For each accepted TCP connection:
  - Spawns a `Connection` task.
  - Performs `HELLO`/`AUTH` handshake.
  - Parses/encodes frames with `encode_frame`/`try_decode_frame`.
  - Routes application frames to a `MessageHandler` (typically `BrokerHandler`).

### `blipmq_core` (Broker)

- `Broker` manages:
  - Topic registry: sharded map `TopicName -> Topic`.
  - Per-topic `Topic` instances with subscribers.
  - Per-subscriber `SubscriberQueue` with:
    - Bounded pending messages.
    - Inflight QoS1 messages with delivery tags.
  - Per-message metadata (TTL, delivery attempts, WAL id).
- Provides operations:
  - `subscribe`, `publish`, `publish_durable`, `poll`, `ack`.
  - `maintenance_tick` for TTL & retry logic.
  - `replay_from_wal` and `flush_wal` when WAL is configured.

### `blipmq_wal` (Write-Ahead Log)

- Append-only file with:
  - Fixed header and `[id][len][crc32][payload...]` records.
  - CRC32 for corruption detection.
  - Index (`id -> offset`) rebuilt on startup.
- `WalConfig` allows tuning fsync policy:
  - `fsync_every_n`.
  - `fsync_interval`.
- Used by `Broker::publish_durable` and `Broker::replay_from_wal`.

### `blipmq_auth` (Auth)

- `ApiKey` newtype.
- `ApiKeyValidator` trait.
- `StaticApiKeyValidator` based on an in-memory set of allowed keys.
- Enforced at the `Connection` level:
  - Only HELLO/AUTH frames are accepted pre-auth.
  - Non-auth frames before authentication yield `NACK(401, "unauthenticated")`.

### `blipmq_metrics` (Metrics)

- HTTP server exposing `GET /metrics`.
- Retrieves metrics from `Broker` and `WriteAheadLog`.
- Suitable for basic monitoring or Prometheus scraping.

### `blipmq_config` (Config)

- Loads configuration from:
  - Optional TOML/YAML file.
  - Environment variables that override file values.
- Provides `Config` used by `blipmqd` to configure:
  - TCP and metrics bind addresses.
  - WAL path and fsync policy.
  - QoS1 retry/backoff settings.
  - Auth keys.
  - `enable_tokio_console`.

### `blipmqd` (Daemon)

- Orchestrates:
  - Config loading.
  - Tracing initialization (`tracing_subscriber` or `tokio-console`).
  - WAL open and replay.
  - Broker construction.
  - Auth validator setup.
  - Network server and metrics server start.
  - Maintenance scheduler.
  - Signal handling and graceful shutdown.

## Dataflow: Publish & Delivery

### Publish Path (Client → Broker)

```text
Client              Net Server            BrokerHandler              Broker               WAL
======              ==========            ============               ======               ===
PUBLISH frame
  (topic, qos, msg)
    |                     |                     |                      |                   |
    |-------------------->|                     |                      |                   |
    |     Connection      |                     |                      |                   |
    |  try_decode_frame   |                     |                      |                   |
    |                     |   handle_frame      |                      |                   |
    |                     |-------------------->|                      |                   |
    |                     |                     | decode PublishPayload|                   |
    |                     |                     | validate QoS/topic   |                   |
    |                     |                     |                      |                   |
    |                     |                     | qos0 or QoS1+no WAL  |                   |
    |                     |                     |--------------------->| publish()         |
    |                     |                     |                      | enqueue           |
    |                     |                     | QoS1 + WAL present   |                   |
    |                     |                     |--------------------->| publish_durable() |
    |                     |                     |                      |   append record   |
    |                     |                     |                      |------------------>|
    |                     |                     |                      | enqueue           |
    |                     |                     | return None          |                   |
    |<--------------------|                     |                      |                   |
  (no response frame)
```

### Subscribe Path (Client → Broker)

```text
Client          Net Server         BrokerHandler               Broker
======          ==========         ============                ======
SUBSCRIBE
 (topic,qos)
   |                  |               |                         |
   |----------------->|               |                         |
   |   Connection     | handle_frame  |                         |
   |                  |-------------> |                         |
   |                  |               | decode SubscribePayload |
   |                  |               | topic, qos validation   |
   |                  |               | create ClientId         |
   |                  |               |------------------------>|
   |                  |               |     subscribe()         |
   |                  |               |<------------------------|
   |                  |               |   SubscriptionId        |
   |                  |               | build AckPayload        |
   |<-----------------|               |                         |
   |   ACK frame      |               |                         |
```

### QoS1 Delivery & ACK Sequence

Once subscribed, the client will poll for messages and send ACKs:

```text
Client             Net Server          BrokerHandler           Broker
======             ==========          ============            ======
POLL
 (sub_id)              |                    |                    |
   |------------------>|                    |                    |
   |   Connection      | handle_frame       |                    |
   |                   |------------------->|                    |
   |                   |                    | decode PollPayload |
   |                   |                    |------------------->|
   |                   |                    |   poll()           |
   |                   |                    |   dequeue QoS1     |
   |                   |                    |   -> DeliveryTag   |
   |                   |                    |<-------------------|
   |                   |                    | build PUBLISH frame|
   |<------------------|                    |                    |
   | PUBLISH (delivery)|                    |                    |

ACK
 (sub_id, correlation_id=DeliveryTag)
   |------------------>|                    |                    |
   |                   | handle_frame       |                    |
   |                   |------------------->|                    |
   |                   |                    | decode AckPayload  |
   |                   |                    | DeliveryTag from   |
   |                   |                    |   frame.correlation|
   |                   |                    |------------------->|
   |                   |                    |   ack(sub_id, tag) |
   |                   |                    |<-------------------|
   |<------------------|                    |                    |
   |   (no frame)      |                    |                    |
```

## In-Memory Queues and WAL (ASCII Layout)

The broker maintains per-subscriber queues, while the WAL holds durable records:

```text
Topic "orders"
  |
  +-- Subscriber sub-1 (client-1)
  |       SubscriberQueue:
  |         pending  : [msg#3, msg#4, ...]
  |         inflight : { tag#5 -> msg#2, tag#6 -> msg#1 }
  |
  +-- Subscriber sub-2 (client-2)
          SubscriberQueue:
            pending  : [msg#3, ...]
            inflight : { tag#7 -> msg#1 }

WAL file:
  [header]
  [id=1][len][crc32][payload(topic="orders", qos=1, msg#1)...]
  [id=2][len][crc32][payload(topic="orders", qos=1, msg#2)...]
  [id=3][len][crc32][payload(topic="orders", qos=1, msg#3)...]
  [id=4][len][crc32][payload(topic="orders", qos=1, msg#4)...]
  ...

In-memory WAL index:
  { 1 -> offset_1, 2 -> offset_2, 3 -> offset_3, 4 -> offset_4, ... }
```

On recovery, the broker:

1. Opens WAL and rebuilds the index.
2. Iterates records and decodes topic/QoS/payload.
3. Re-enqueues messages into subscriber queues as if they were freshly published.

QoS1 semantics:

- Messages that were never ACKed before crash will be redelivered.
- Messages that were ACKed may also be redelivered (at-least-once).

## Scheduler & Maintenance

The broker does not use background threads internally; instead, `blipmqd` runs a Tokio task that periodically calls `Broker::maintenance_tick`:

```text
[Tokio interval]
    |
    v
Broker::maintenance_tick(now)
    ├─ iterate subscriptions
    ├─ call SubscriberQueue::maintenance_tick
    │    ├─ drop expired messages from pending
    │    ├─ drop expired messages from inflight
    │    └─ reschedule or drop inflight QoS1 based on retry policy
    └─ update internal counters
```

This decouples time-based behavior (TTL expiry, retries) from the main publish/poll paths, keeping hot paths lean.
