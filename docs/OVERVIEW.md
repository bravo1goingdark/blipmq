# BlipMQ Overview

BlipMQ is a lightweight, high-performance message broker written in Rust. It is designed for low-latency pub/sub workloads with a simple binary protocol over TCP, durable write-ahead logging (WAL), and operational clarity.

## Goals

- **Fast**: Async I/O with Tokio, minimal copying using `Bytes`/`BytesMut`, and sharded locks in the broker core.
- **Durable**: Append-only WAL with CRC32 and index rebuild on startup.
- **Reliable delivery**: QoS0 (at-most-once) and QoS1 (at-least-once) with retry and TTL.
- **Operationally simple**: Static API-key auth, text metrics endpoint, config via file + env overrides.
- **Observable**: Tracing spans, optional `tokio-console` integration, benchmark harness, and chaos tools.

## Workspace Overview

The BlipMQ repository is a Cargo workspace composed of focused crates:

- `blipmqd` – main daemon (TCP server, WAL, auth, metrics, graceful shutdown).
- `core` – broker core (topics, per-subscriber queues, QoS, TTL, retry).
- `net` – TCP networking and binary framing (HELLO/AUTH, PUBLISH/SUBSCRIBE, POLL/ACK).
- `wal` – write-ahead log for durable QoS1 messages.
- `auth` – static API key validation.
- `metrics` – HTTP `/metrics` endpoint.
- `config` – configuration loader (TOML/YAML + env overrides).
- `bench` – in-process benchmark harness using the real protocol.
- `chaos` – chaos/failure testing utilities.

## Feature Summary

- Topics with per-subscriber queues.
- QoS0 and QoS1 delivery semantics.
- Per-message TTL and retry with exponential backoff.
- WAL-backed durable topics with crash recovery via replay.
- Static API-key auth enforced at the connection layer.
- HTTP metrics for topics, subscribers, delivered/published messages, and WAL statistics.
- Configurable WAL fsync policy (`fsync_every_n`, `fsync_interval`).
- Optional `tokio-console` integration for async profiling.
- Benchmark and chaos testing crates for performance and reliability work.

## Minimal Rust Publish/Subscribe Client

The example below shows a minimal async client using the public API from `net`:

- Connects to a running `blipmqd` at `127.0.0.1:5555`.
- Performs `HELLO` and `AUTH` with an API key.
- Subscribes to topic `demo` with QoS1.
- Publishes a message to `demo`.
- Polls for the delivery and ACKs it.

### Prerequisites

Run `blipmqd` with a configuration that includes at least one allowed API key, for example:

```toml
bind_addr = "127.0.0.1"
port = 5555

metrics_addr = "127.0.0.1"
metrics_port = 9100

wal_path = "blipmq.wal"
fsync_policy = "every_n:64"

max_retries = 3
retry_backoff_ms = 100

allowed_api_keys = ["dev-key-1"]
```

### Client Cargo.toml

Create a standalone Rust binary project and add:

```toml
[package]
name = "blipmq-example-client"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net", "io-util", "time"] }
bytes = "1"
net = { path = "../net" } # adjust path as needed
```

### Client Example (`src/main.rs`)

```rust
use std::error::Error;
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use net::{
    AckPayload, AuthPayload, Frame, FrameType, HelloPayload,
    NackPayload, PollPayload, PublishPayload, SubscribePayload,
    PROTOCOL_VERSION, encode_frame, try_decode_frame,
};

const API_KEY: &str = "dev-key-1";

async fn read_frame(
    stream: &mut TcpStream,
    buf: &mut BytesMut,
) -> Result<Option<Frame>, Box<dyn Error>> {
    loop {
        if let Some(frame) = try_decode_frame(buf)? {
            return Ok(Some(frame));
        }

        let n = stream.read_buf(buf).await?;
        if n == 0 {
            return Ok(None); // connection closed
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let addr = "127.0.0.1:5555";
    let mut stream = TcpStream::connect(addr).await?;
    let mut buf = BytesMut::with_capacity(4096);
    let mut correlation: u64 = 1;

    // 1) HELLO
    let hello_payload = HelloPayload {
        protocol_version: PROTOCOL_VERSION,
    }
    .encode();
    let hello_frame = Frame {
        msg_type: FrameType::Hello,
        correlation_id: correlation,
        payload: hello_payload,
    };
    correlation += 1;

    let mut out = BytesMut::new();
    encode_frame(&hello_frame, &mut out)?;
    stream.write_all(&out).await?;
    out.clear();

    let resp = read_frame(&mut stream, &mut buf)
        .await?
        .expect("connection closed during HELLO");
    match resp.msg_type {
        FrameType::Ack => {
            let ack = AckPayload::decode(&resp.payload)?;
            assert_eq!(ack.subscription_id, 0);
        }
        FrameType::Nack => {
            let nack = NackPayload::decode(&resp.payload)?;
            panic!(
                "HELLO rejected: code={} message={}",
                nack.code, nack.message
            );
        }
        other => panic!("unexpected response to HELLO: {:?}", other),
    }

    // 2) AUTH
    let auth_payload = AuthPayload {
        api_key: API_KEY.to_string(),
    }
    .encode()?;
    let auth_frame = Frame {
        msg_type: FrameType::Auth,
        correlation_id: correlation,
        payload: auth_payload,
    };
    correlation += 1;

    encode_frame(&auth_frame, &mut out)?;
    stream.write_all(&out).await?;
    out.clear();

    let resp = read_frame(&mut stream, &mut buf)
        .await?
        .expect("connection closed during AUTH");
    match resp.msg_type {
        FrameType::Ack => {}
        FrameType::Nack => {
            let nack = NackPayload::decode(&resp.payload)?;
            panic!(
                "AUTH rejected: code={} message={}",
                nack.code, nack.message
            );
        }
        other => panic!("unexpected response to AUTH: {:?}", other),
    }

    // 3) SUBSCRIBE to "demo" with QoS1
    let subscribe_payload = SubscribePayload {
        topic: "demo".to_string(),
        qos: 1,
    }
    .encode()?;
    let subscribe_frame = Frame {
        msg_type: FrameType::Subscribe,
        correlation_id: correlation,
        payload: subscribe_payload,
    };
    correlation += 1;

    encode_frame(&subscribe_frame, &mut out)?;
    stream.write_all(&out).await?;
    out.clear();

    let resp = read_frame(&mut stream, &mut buf)
        .await?
        .expect("connection closed during SUBSCRIBE");
    let sub_id = match resp.msg_type {
        FrameType::Ack => {
            let ack = AckPayload::decode(&resp.payload)?;
            if ack.subscription_id == 0 {
                panic!("broker returned invalid subscription_id=0");
            }
            ack.subscription_id
        }
        FrameType::Nack => {
            let nack = NackPayload::decode(&resp.payload)?;
            panic!(
                "SUBSCRIBE rejected: code={} message={}",
                nack.code, nack.message
            );
        }
        other => panic!("unexpected response to SUBSCRIBE: {:?}", other),
    };

    // 4) PUBLISH a QoS1 message to "demo"
    let message = b"hello from client".as_ref().into();
    let publish_payload = PublishPayload {
        topic: "demo".to_string(),
        qos: 1,
        message,
    }
    .encode()?;
    let publish_frame = Frame {
        msg_type: FrameType::Publish,
        correlation_id: correlation,
        payload: publish_payload,
    };
    correlation += 1;

    encode_frame(&publish_frame, &mut out)?;
    stream.write_all(&out).await?;
    out.clear();

    // 5) POLL for the message
    let poll_payload = PollPayload {
        subscription_id: sub_id,
    }
    .encode()?;
    let poll_correlation = correlation;
    correlation += 1;

    let poll_frame = Frame {
        msg_type: FrameType::Poll,
        correlation_id: poll_correlation,
        payload: poll_payload,
    };
    encode_frame(&poll_frame, &mut out)?;
    stream.write_all(&out).await?;
    out.clear();

    // Wait up to 5 seconds for delivery
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let delivery_frame = loop {
        if let Some(frame) = read_frame(&mut stream, &mut buf).await? {
            match frame.msg_type {
                FrameType::Publish => break frame,
                FrameType::Nack => {
                    let nack = NackPayload::decode(&frame.payload)?;
                    panic!(
                        "POLL NACK: code={} message={}",
                        nack.code, nack.message
                    );
                }
                _ => {
                    // Ignore unrelated frames.
                }
            }
        }

        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for delivery");
        }
    };

    let delivery_payload = PublishPayload::decode(&delivery_frame.payload)?;
    println!(
        "Received message on topic {:?}: {:?}",
        delivery_payload.topic,
        String::from_utf8_lossy(&delivery_payload.message)
    );

    // 6) ACK the delivered message using the delivery correlation id as tag
    let ack_payload = AckPayload {
        subscription_id: sub_id,
    }
    .encode()?;
    let ack_frame = Frame {
        msg_type: FrameType::Ack,
        correlation_id: delivery_frame.correlation_id,
        payload: ack_payload,
    };
    encode_frame(&ack_frame, &mut out)?;
    stream.write_all(&out).await?;
    out.clear();

    println!("ACK sent, done.");
    Ok(())
}
```

This example uses the real protocol and payload types from `net` and will successfully publish, receive, and ACK a message when `blipmqd` is running and configured with the appropriate API key.

