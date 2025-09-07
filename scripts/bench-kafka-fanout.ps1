# Kafka fanout benchmark: 8 consumers measuring delivered throughput
param(
  [string]$Msgs = "2000",
  [string]$Size = "100",
  [string]$Consumers = "8"
)

$ErrorActionPreference = "Stop"

# Ensure a dedicated docker network
try { docker network create bench-net | Out-Null } catch { }

# Start single-node Kafka (KRaft mode)
docker run -d --name kafka --network bench-net -p 9092:9092 `
  -e KAFKA_ENABLE_KRAFT=yes `
  -e KAFKA_CFG_NODE_ID=1 `
  -e KAFKA_CFG_PROCESS_ROLES=broker,controller `
  -e KAFKA_CFG_LISTENERS=PLAINTEXT://:9092,CONTROLLER://:9093 `
  -e KAFKA_CFG_ADVERTISED_LISTENERS=PLAINTEXT://kafka:9092 `
  -e KAFKA_CFG_CONTROLLER_LISTENER_NAMES=CONTROLLER `
  -e KAFKA_CFG_CONTROLLER_QUORUM_VOTERS=1@kafka:9093 `
  bitnami/kafka:latest | Out-Null
Start-Sleep -Seconds 15  # Extended wait for KRaft setup

try {
  # Create topic with multiple partitions to enable parallel consumption
  docker exec kafka /opt/bitnami/kafka/bin/kafka-topics.sh --bootstrap-server kafka:9092 --create --topic bench-fanout --partitions $Consumers --replication-factor 1 | Out-Null
  Start-Sleep -Seconds 2

  Write-Host "Starting Kafka fanout test: $Msgs msgs × $Consumers consumers = $([int]$Msgs * [int]$Consumers) total deliveries"

  # Start consumer perf test in background
  $consumerSw = [System.Diagnostics.Stopwatch]::StartNew()
  
  # Start consumers in background (they will consume from different partitions)
  $consumerJobs = @()
  for ($i = 0; $i -lt [int]$Consumers; $i++) {
    $consumerJob = Start-Job -ScriptBlock {
      param($consumerIndex, $msgsPerConsumer)
      # Each consumer gets a unique group so they all receive all messages
      $consumerGroup = "bench-group-$consumerIndex"
      $result = docker exec kafka /opt/bitnami/kafka/bin/kafka-console-consumer.sh `
        --bootstrap-server kafka:9092 `
        --topic bench-fanout `
        --group $consumerGroup `
        --max-messages $msgsPerConsumer 2>&1
      
      # Count actual messages received (filter out metadata)
      $msgCount = ($result | Where-Object { $_ -match "^x+$" }).Count
      return @{
        ConsumerIndex = $consumerIndex
        Received = $msgCount
        Output = $result
      }
    } -ArgumentList $i, $Msgs
    
    $consumerJobs += $consumerJob
    Start-Sleep -Milliseconds 500
  }

  # Let consumers settle
  Start-Sleep -Seconds 3

  # Produce messages
  $producerSw = [System.Diagnostics.Stopwatch]::StartNew()
  docker exec kafka /opt/bitnami/kafka/bin/kafka-producer-perf-test.sh --topic bench-fanout --num-records $Msgs --record-size $Size --throughput -1 --producer-props bootstrap.servers=kafka:9092 acks=1 | Out-Null
  $producerSw.Stop()

  # Wait for consumers to complete and collect results
  $consumerResults = @()
  foreach ($job in $consumerJobs) {
    $result = Receive-Job -Job $job -Wait -Timeout 60
    if ($result) {
      $consumerResults += $result
    }
    Remove-Job -Job $job
  }
  
  $consumerSw.Stop()
  
  # Calculate results
  $totalReceived = ($consumerResults | Measure-Object -Property Received -Sum).Sum
  $expectedTotal = ([int]$Msgs) * ([int]$Consumers)
  $deliveredThroughput = $totalReceived / $consumerSw.Elapsed.TotalSeconds
  $deliveredMBps = ($totalReceived * [int]$Size) / $consumerSw.Elapsed.TotalSeconds / 1048576.0
  
  Write-Host "Kafka fanout results:"
  Write-Host "  Delivered throughput: $([Math]::Round($deliveredThroughput, 2)) msgs/sec"
  Write-Host "  Delivered bandwidth: $([Math]::Round($deliveredMBps, 2)) MB/sec"
  Write-Host "  Total messages received: $totalReceived / $expectedTotal"
  Write-Host "  Producer time: $([Math]::Round($producerSw.Elapsed.TotalSeconds, 2))s"
  Write-Host "  Consumer time: $([Math]::Round($consumerSw.Elapsed.TotalSeconds, 2))s"
  
  # Show per-consumer breakdown
  Write-Host "  Per-consumer results:"
  foreach ($result in $consumerResults) {
    Write-Host "    Consumer $($result.ConsumerIndex): $($result.Received) msgs"
  }

} finally {
  # Cleanup jobs
  Get-Job | Remove-Job -Force -ErrorAction SilentlyContinue
  docker rm -f kafka | Out-Null
}
