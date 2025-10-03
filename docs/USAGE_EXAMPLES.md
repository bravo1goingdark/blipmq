# BlipMQ Usage Examples

## 🚀 Quick Start Examples

### Basic Pub/Sub
```bash
# Terminal 1: Start the broker
blipmq start

# Terminal 2: Subscribe to messages
blipmq-cli sub chat

# Terminal 3: Publish messages
blipmq-cli pub chat "Hello, World!"
blipmq-cli pub chat "How are you doing?"
```

### Advanced Publishing
```bash
# Publish with custom TTL (5 seconds)
blipmq-cli pub events "Server started" --ttl 5000

# Publish from file
blipmq-cli pub logs --file /var/log/app.log

# Publish from stdin
echo "Dynamic message" | blipmq-cli pub notifications -

# Publish binary data from file
blipmq-cli pub images --file photo.jpg --ttl 30000
```

### Advanced Subscribing
```bash
# Subscribe and exit after 10 messages
blipmq-cli sub events --count 10

# Subscribe with JSON output format
blipmq-cli sub events --format json

# Subscribe with quiet mode (messages only)
blipmq-cli -q sub events

# Subscribe with verbose logging
blipmq-cli -v sub events

# Subscribe to multiple topics (run in parallel)
blipmq-cli sub chat &
blipmq-cli sub events &
blipmq-cli sub logs &
```

## 🏗️ Server Operations

### Configuration Management
```bash
# Start with development config
blipmq start --config config/blipmq-dev.toml

# Start with production config
blipmq start --config config/blipmq-production.toml

# Start with ultra-performance config
blipmq start --config config/blipmq-ultra.toml

# Override config via environment variable
export BLIPMQ_CONFIG=/path/to/custom.toml
blipmq start
```

### Interactive Mode
```bash
# Connect in interactive mode
blipmq connect 127.0.0.1:8080

# Available commands in interactive mode:
> help
> pub chat "Hello from interactive mode"
> sub events
> unsub events
> exit
```

## 💼 Production Use Cases

### Microservices Communication
```bash
# Service A publishes events
blipmq-cli --addr prod-broker:7878 pub user.created '{"id":123,"name":"Alice"}'

# Service B subscribes to user events
blipmq-cli --addr prod-broker:7878 sub user.* --format json
```

### Log Aggregation
```bash
# Ship application logs
tail -f /var/log/app.log | while read line; do
  blipmq-cli pub logs.app "$line" --ttl 300000
done

# Central log collector
blipmq-cli sub logs.* --format json | jq '.'
```

### IoT Data Collection
```bash
# Sensor data publishing
blipmq-cli pub sensor.temperature "23.5" --ttl 60000
blipmq-cli pub sensor.humidity "65.2" --ttl 60000

# Data processing service
blipmq-cli sub sensor.* --format json --count 1000 | process_sensor_data.py
```

### Event Sourcing
```bash
# Publish domain events
blipmq-cli pub events.order-created '{"orderId":"12345","amount":99.99}' --ttl 0
blipmq-cli pub events.payment-processed '{"orderId":"12345","status":"success"}' --ttl 0

# Event store replay
blipmq-cli sub events.* --format json | event_store_writer.py
```

### Real-time Notifications
```bash
# Publish notifications with short TTL
blipmq-cli pub notifications.user.123 "New message received" --ttl 30000

# Mobile app subscription
blipmq-cli sub notifications.user.* --format json | notification_handler.py
```

## 🐍 Programming Language Examples

### Python Client Example
```python
import socket
import struct
import json

class BlipMQClient:
    def __init__(self, host='127.0.0.1', port=8080):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.connect((host, port))
    
    def publish(self, topic, message, ttl=60000):
        # Simplified example - actual implementation would use FlatBuffers
        data = json.dumps({
            "action": "pub",
            "topic": topic,
            "message": message,
            "ttl": ttl
        }).encode('utf-8')
        
        # Send length prefix + data
        self.sock.send(struct.pack('>I', len(data)))
        self.sock.send(data)
    
    def close(self):
        self.sock.close()

# Usage
client = BlipMQClient()
client.publish("events", "Hello from Python!")
client.close()
```

### Node.js Client Example
```javascript
const net = require('net');

class BlipMQClient {
    constructor(host = '127.0.0.1', port = 8080) {
        this.client = new net.Socket();
        this.client.connect(port, host);
    }
    
    publish(topic, message, ttl = 60000) {
        // Simplified example - actual implementation would use FlatBuffers
        const data = Buffer.from(JSON.stringify({
            action: 'pub',
            topic: topic,
            message: message,
            ttl: ttl
        }));
        
        // Send length prefix + data
        const lengthBuffer = Buffer.allocUnsafe(4);
        lengthBuffer.writeUInt32BE(data.length);
        
        this.client.write(Buffer.concat([lengthBuffer, data]));
    }
    
    close() {
        this.client.destroy();
    }
}

// Usage
const client = new BlipMQClient();
client.publish('events', 'Hello from Node.js!');
client.close();
```

### Go Client Example
```go
package main

import (
    "encoding/binary"
    "encoding/json"
    "net"
)

type BlipMQClient struct {
    conn net.Conn
}

func NewBlipMQClient(host string, port string) (*BlipMQClient, error) {
    conn, err := net.Dial("tcp", host+":"+port)
    if err != nil {
        return nil, err
    }
    return &BlipMQClient{conn: conn}, nil
}

func (c *BlipMQClient) Publish(topic, message string, ttl int) error {
    // Simplified example - actual implementation would use FlatBuffers
    data := map[string]interface{}{
        "action":  "pub",
        "topic":   topic,
        "message": message,
        "ttl":     ttl,
    }
    
    jsonData, err := json.Marshal(data)
    if err != nil {
        return err
    }
    
    // Send length prefix + data
    lengthBytes := make([]byte, 4)
    binary.BigEndian.PutUint32(lengthBytes, uint32(len(jsonData)))
    
    _, err = c.conn.Write(append(lengthBytes, jsonData...))
    return err
}

func (c *BlipMQClient) Close() error {
    return c.conn.Close()
}

// Usage
func main() {
    client, err := NewBlipMQClient("127.0.0.1", "8080")
    if err != nil {
        panic(err)
    }
    defer client.Close()
    
    client.Publish("events", "Hello from Go!", 60000)
}
```

## 📊 Monitoring Examples

### Prometheus Metrics Collection
```bash
# Scrape metrics for monitoring
curl -s http://localhost:9090/metrics | grep blipmq_

# Monitor specific metrics
watch -n 1 'curl -s http://localhost:9090/metrics | grep -E "(published|enqueued|dropped)"'
```

### Health Check Scripts
```bash
#!/bin/bash
# health_check.sh
set -e

# Check if broker is responding
blipmq-cli pub healthcheck "ping" --ttl 5000 > /dev/null

# Check metrics endpoint
curl -f -s http://localhost:9090/metrics > /dev/null

# Check memory usage (under 1GB)
MEMORY_KB=$(ps -o rss= -p $(pgrep blipmq))
if [ $MEMORY_KB -gt 1048576 ]; then
    echo "WARNING: High memory usage: ${MEMORY_KB}KB"
    exit 1
fi

echo "BlipMQ health check passed"
```

### Log Analysis
```bash
# Monitor error rates
journalctl -f -u blipmq | grep -i error | ts '[%Y-%m-%d %H:%M:%S]'

# Monitor performance metrics
journalctl -u blipmq --since "5 minutes ago" | grep -E "(throughput|latency)"

# Extract performance statistics
journalctl -u blipmq --since "1 hour ago" | \
  grep "messages/sec" | \
  awk '{print $6}' | \
  sort -n | \
  tail -10
```

## 🔧 Troubleshooting Examples

### Connection Issues
```bash
# Test basic connectivity
telnet 127.0.0.1 8080

# Check if port is open
nmap -p 8080 127.0.0.1

# Debug connection with verbose CLI
blipmq-cli -v pub test "debug message"
```

### Performance Debugging
```bash
# Monitor resource usage
top -p $(pgrep blipmq) -n 1

# Check network connections
netstat -an | grep 8080

# Monitor disk I/O
iotop -p $(pgrep blipmq) -o -d 1

# Check memory pools
curl -s http://localhost:9090/metrics | grep pool_
```

### Load Testing
```bash
#!/bin/bash
# load_test.sh
TOPIC="loadtest"
MESSAGES=10000
PARALLEL=10

echo "Starting load test: $MESSAGES messages with $PARALLEL publishers"

# Start background subscribers
for i in $(seq 1 3); do
    blipmq-cli sub $TOPIC --count $(($MESSAGES / 3)) --format json > /dev/null &
done

# Start parallel publishers
for i in $(seq 1 $PARALLEL); do
    (
        for j in $(seq 1 $(($MESSAGES / $PARALLEL))); do
            blipmq-cli pub $TOPIC "Message $i-$j from publisher $i" --ttl 30000
        done
    ) &
done

wait
echo "Load test completed"
```

## 🚀 Advanced Patterns

### Message Routing
```bash
# Route messages based on content
blipmq-cli pub logs.error "Critical system failure" --ttl 300000
blipmq-cli pub logs.info "System startup complete" --ttl 60000
blipmq-cli pub logs.debug "Debug trace information" --ttl 10000

# Different services subscribe to different log levels
blipmq-cli sub logs.error    # Alert service
blipmq-cli sub logs.info     # Monitoring service
blipmq-cli sub logs.*        # Log aggregation service
```

### Fan-out Broadcasting
```bash
# Single publisher, multiple subscribers
# Terminal 1: Publisher
while true; do
    blipmq-cli pub broadcast "$(date): System status update"
    sleep 5
done

# Multiple terminals: Subscribers
blipmq-cli sub broadcast  # Service A
blipmq-cli sub broadcast  # Service B  
blipmq-cli sub broadcast  # Service C
```

### Request-Response Pattern
```bash
# Service A: Send request and wait for response
REQUEST_ID=$(uuidgen)
blipmq-cli pub requests.process "{'id':'$REQUEST_ID','data':'...'}" --ttl 30000 &
blipmq-cli sub responses.$REQUEST_ID --count 1 --ttl 30000

# Service B: Process requests and send responses
blipmq-cli sub requests.* --format json | while read request; do
    REQUEST_ID=$(echo $request | jq -r '.id')
    RESULT="processed_$(date +%s)"
    blipmq-cli pub responses.$REQUEST_ID "$RESULT" --ttl 60000
done
```

### Event Sourcing with Snapshots
```bash
# Publish events with increasing sequence numbers
SEQ=1
while read event; do
    blipmq-cli pub events.stream "{'seq':$SEQ,'event':'$event'}" --ttl 0
    ((SEQ++))
done

# Create periodic snapshots
if [ $((SEQ % 100)) -eq 0 ]; then
    blipmq-cli pub snapshots.latest "{'seq':$SEQ,'state':'...'}" --ttl 0
fi
```

## 🎯 Performance Tuning Examples

### Batch Operations
```bash
# Batch publish for higher throughput
{
    echo "message 1"
    echo "message 2" 
    echo "message 3"
} | while read msg; do
    blipmq-cli pub batch-topic "$msg" --ttl 60000
done

# Process in batches
blipmq-cli sub batch-topic --count 100 --format json | \
    jq -s '.' | \
    process_batch.py
```

### Memory-Mapped File Publishing
```bash
# Publish large files efficiently
blipmq-cli pub file-transfer --file large_dataset.json --ttl 300000

# Stream processing
mkfifo /tmp/stream
tail -f /var/log/stream.log > /tmp/stream &
blipmq-cli pub stream - < /tmp/stream
```

### Multi-topic Aggregation
```bash
# Aggregate from multiple topics
(
    blipmq-cli sub topic1 --format json | sed 's/^/topic1: /' &
    blipmq-cli sub topic2 --format json | sed 's/^/topic2: /' &
    blipmq-cli sub topic3 --format json | sed 's/^/topic3: /' &
    wait
) | sort_and_aggregate.py
```