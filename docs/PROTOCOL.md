# BlipMQ Protocol Specification

This document describes the BlipMQ wire protocol, message formats, and client-server communication patterns.

## Overview

BlipMQ uses a TCP-based protocol with FlatBuffers for message serialization. This provides:
- Efficient binary serialization with zero-copy deserialization
- Language-agnostic schema evolution
- Small message sizes and fast parsing
- Type safety and schema validation

## Connection Flow

```
Client                          Server
  |                               |
  |-------- TCP Connect -------->|
  |<------- Accept Connection ----|
  |                               |
  |-------- Subscribe Req ------>|
  |<------- Subscribe Ack -------|
  |                               |
  |-------- Publish Msg ------->| 
  |<------- Publish Ack --------|
  |                               |
  |<------- Message Delivery ----|
  |-------- Message Ack -------->|
  |                               |
  |-------- Close Connection --->|
  |<------- Connection Closed ---|
```

## FlatBuffers Schema

The BlipMQ protocol is defined using the following FlatBuffers schema:

```flatbuf
// BlipMQ Protocol Schema
// File: protocol.fbs

namespace blipmq;

// Message types
union MessageType {
    Subscribe,
    Unsubscribe,
    Publish,
    Message,
    Ack,
    Ping,
    Pong,
    Error
}

// Quality of Service levels
enum QoSLevel : byte {
    AtMostOnce = 0,     // Fire and forget
    AtLeastOnce = 1     // Guaranteed delivery
}

// Error codes
enum ErrorCode : byte {
    None = 0,
    InvalidTopic = 1,
    TopicNotFound = 2,
    MessageTooLarge = 3,
    QueueFull = 4,
    AuthenticationFailed = 5,
    AuthorizationFailed = 6,
    ServerError = 7,
    ProtocolError = 8,
    TTLExpired = 9
}

// Subscribe to a topic
table Subscribe {
    topic: string (required);
    qos: QoSLevel = AtMostOnce;
    client_id: string;
}

// Unsubscribe from a topic
table Unsubscribe {
    topic: string (required);
    client_id: string;
}

// Publish a message to a topic
table Publish {
    topic: string (required);
    payload: [ubyte] (required);
    qos: QoSLevel = AtMostOnce;
    ttl_ms: uint64 = 0;         // 0 = no expiration
    message_id: string;         // Required for QoS 1
}

// Message delivery from server to client
table Message {
    topic: string (required);
    payload: [ubyte] (required);
    message_id: string;         // For acknowledgment
    timestamp: uint64;          // Unix timestamp
    qos: QoSLevel;
}

// Acknowledgment message
table Ack {
    message_id: string (required);
    error_code: ErrorCode = None;
}

// Ping message (keepalive)
table Ping {
    timestamp: uint64;
}

// Pong message (keepalive response)
table Pong {
    timestamp: uint64;
}

// Error message
table Error {
    error_code: ErrorCode (required);
    message: string;
    details: string;
}

// Main protocol message wrapper
table ProtocolMessage {
    message: MessageType (required);
    sequence_number: uint64;
    timestamp: uint64;
}

root_type ProtocolMessage;
```

## Message Format

All messages are wrapped in a `ProtocolMessage` envelope:

```
+------------------+------------------+
| Message Length   | FlatBuffer Data  |
| (4 bytes)        | (Variable)       |
+------------------+------------------+
```

- **Message Length**: 4-byte little-endian integer indicating the size of the FlatBuffer data
- **FlatBuffer Data**: Serialized `ProtocolMessage` containing the actual message

## Message Types

### Subscribe Message

Subscribe to receive messages from a topic.

```json
{
  "message": {
    "Subscribe": {
      "topic": "sensor/temperature",
      "qos": "AtMostOnce",
      "client_id": "client_001"
    }
  },
  "sequence_number": 1,
  "timestamp": 1672531200
}
```

### Publish Message

Publish a message to a topic.

```json
{
  "message": {
    "Publish": {
      "topic": "sensor/temperature",
      "payload": [22, 46, 67, 101, 108, 115, 105, 117, 115],
      "qos": "AtMostOnce",
      "ttl_ms": 30000,
      "message_id": "msg_12345"
    }
  },
  "sequence_number": 2,
  "timestamp": 1672531201
}
```

### Message Delivery

Server delivers a message to subscribed clients.

```json
{
  "message": {
    "Message": {
      "topic": "sensor/temperature",
      "payload": [22, 46, 67, 101, 108, 115, 105, 117, 115],
      "message_id": "msg_12345",
      "timestamp": 1672531201,
      "qos": "AtMostOnce"
    }
  },
  "sequence_number": 3,
  "timestamp": 1672531201
}
```

### Acknowledgment

Acknowledge receipt of a message (QoS 1 only).

```json
{
  "message": {
    "Ack": {
      "message_id": "msg_12345",
      "error_code": "None"
    }
  },
  "sequence_number": 4,
  "timestamp": 1672531202
}
```

## Quality of Service (QoS)

BlipMQ supports two QoS levels:

### QoS 0: At Most Once
- Fire-and-forget delivery
- No acknowledgments required
- Minimal overhead, maximum throughput
- Messages may be lost if subscriber is unavailable

### QoS 1: At Least Once
- Guaranteed delivery with acknowledgments
- Messages are stored until acknowledged
- Higher overhead due to ACK mechanism
- Messages may be delivered multiple times

## Topic Naming

Topics are UTF-8 strings with the following rules:
- Maximum length: 255 characters
- Case-sensitive
- Support hierarchical structure with `/` separator
- No wildcards in publish topics
- Subscribe supports basic pattern matching (planned feature)

Examples:
- `sensor/temperature`
- `logs/application/error`
- `events/user/login`
- `metrics/cpu/usage`

## Connection Management

### Keepalive

Clients should send periodic `Ping` messages to maintain connection:
- Default interval: 60 seconds
- Server responds with `Pong`
- Connection is closed if no ping received within 2x interval

### Graceful Shutdown

Clients should:
1. Send `Unsubscribe` for all active subscriptions
2. Wait for acknowledgments
3. Close TCP connection

Servers will:
1. Process remaining messages in queues
2. Send final message deliveries
3. Close connection

## Error Handling

### Error Codes

| Code | Name | Description |
|------|------|-------------|
| 0 | None | No error |
| 1 | InvalidTopic | Topic name is invalid |
| 2 | TopicNotFound | Topic does not exist |
| 3 | MessageTooLarge | Message exceeds size limit |
| 4 | QueueFull | Subscriber queue is full |
| 5 | AuthenticationFailed | Invalid credentials |
| 6 | AuthorizationFailed | Access denied |
| 7 | ServerError | Internal server error |
| 8 | ProtocolError | Protocol violation |
| 9 | TTLExpired | Message expired |

### Error Response

```json
{
  "message": {
    "Error": {
      "error_code": "MessageTooLarge",
      "message": "Message size exceeds limit",
      "details": "Message size: 2MB, Limit: 1MB"
    }
  },
  "sequence_number": 5,
  "timestamp": 1672531203
}
```

## Authentication (Optional)

When authentication is enabled:

1. Client connects and sends first message with API key
2. Server validates key and responds with Ack or Error
3. All subsequent messages are authenticated based on connection

API key can be included in any message's metadata (implementation-specific).

## Performance Considerations

### Message Batching

- Server batches messages for delivery efficiency
- Configurable batch size and flush interval
- Automatic flushing on batch size or time limits

### Connection Pooling

- Clients can maintain multiple connections for parallelism
- Each connection maintains independent subscription state
- Load balancing across connections for high throughput

### Memory Management

- Zero-copy deserialization with FlatBuffers
- Message pooling for frequent allocations
- Bounded queues prevent memory exhaustion

## Client Libraries

### Rust Client Example

```rust
use blipmq::{Client, Message, QoSLevel};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connect to broker
    let client = Client::connect("127.0.0.1:7878").await?;
    
    // Subscribe to topic
    client.subscribe("sensor/temperature", QoSLevel::AtMostOnce).await?;
    
    // Publish message
    let payload = b"22.5";
    client.publish(
        "sensor/temperature", 
        payload, 
        QoSLevel::AtMostOnce, 
        Duration::from_secs(30)
    ).await?;
    
    // Receive messages
    while let Ok(message) = client.receive().await {
        println!("Received: {} from {}", 
                String::from_utf8_lossy(&message.payload), 
                message.topic);
    }
    
    Ok(())
}
```

## Wire Protocol Example

Here's a hex dump of a simple Publish message:

```
Length: 00000030 (48 bytes)
Data:   
0000: 10 00 00 00 00 00 01 00 04 00 00 00 01 00 00 00  ................
0010: 0C 00 00 00 63 68 61 74 00 00 00 00 05 00 00 00  ....chat........
0020: 48 65 6C 6C 6F 00 00 00 00 00 00 00 00 00 00 00  Hello...........

Decoded:
- MessageType: Publish (1)
- Topic: "chat" 
- Payload: "Hello"
- QoS: AtMostOnce (0)
- TTL: 0 (no expiration)
```

## Extensions and Future Features

### Planned Enhancements

1. **Topic Patterns**: Wildcard subscriptions (`sensor/+/temperature`)
2. **Message Filtering**: Server-side content filtering
3. **Compression**: Optional payload compression (gzip, lz4)
4. **Encryption**: TLS support for secure connections
5. **Clustering**: Multi-broker federation
6. **Dead Letter Queue**: Failed message handling

### Custom Extensions

The protocol supports custom extensions through:
- Reserved error codes (100-255)
- Custom message metadata
- Extensible FlatBuffers schema

## Implementation Notes

### Server Implementation

- Async I/O with tokio runtime
- Per-connection message queues
- Background thread for message fanout
- Lock-free data structures where possible

### Client Implementation

- Automatic reconnection with exponential backoff
- Message acknowledgment tracking for QoS 1
- Connection multiplexing support
- Configurable timeouts and retry policies

For implementation details, see the source code in the `src/` directory.