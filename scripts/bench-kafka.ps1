# Benchmark Kafka throughput using only Docker (no host CLI deps)
param(
  [string]$Msgs = "200000",
  [string]$Size = "100"
)

$ErrorActionPreference = "Stop"

# Ensure a dedicated docker network so containers can resolve each other by name
try { docker network create bench-net | Out-Null } catch { }

# Start single-node Kafka (KRaft mode) on a user-defined network
docker run -d --name kafka --network bench-net -p 9092:9092 `
  -e KAFKA_ENABLE_KRAFT=yes `
  -e KAFKA_CFG_NODE_ID=1 `
  -e KAFKA_CFG_PROCESS_ROLES=broker,controller `
  -e KAFKA_CFG_LISTENERS=PLAINTEXT://:9092,CONTROLLER://:9093 `
  -e KAFKA_CFG_ADVERTISED_LISTENERS=PLAINTEXT://kafka:9092 `
  -e KAFKA_CFG_CONTROLLER_LISTENER_NAMES=CONTROLLER `
  -e KAFKA_CFG_CONTROLLER_QUORUM_VOTERS=1@kafka:9093 `
  bitnami/kafka:latest | Out-Null
Start-Sleep -Seconds 10

try {
  # Run Kafka CLI tools inside the Kafka container via docker exec.
  docker exec kafka /opt/bitnami/kafka/bin/kafka-topics.sh --bootstrap-server kafka:9092 --create --topic bench --partitions 1 --replication-factor 1
  docker exec kafka /opt/bitnami/kafka/bin/kafka-producer-perf-test.sh --topic bench --num-records $Msgs --record-size $Size --throughput -1 --producer-props bootstrap.servers=kafka:9092 acks=0 linger.ms=1 batch.size=131072
} finally {
  docker rm -f kafka | Out-Null
}
