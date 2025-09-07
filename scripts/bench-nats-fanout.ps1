# NATS fanout benchmark: 8 subscribers measuring delivered throughput
param(
  [string]$Msgs = "2000",
  [string]$Size = "100",
  [string]$Subs = "8"
)

$ErrorActionPreference = "Stop"

# Ensure a dedicated docker network
try { docker network create bench-net | Out-Null } catch { }

# Start NATS server
docker run --rm -d --name nats-bench --network bench-net -p 4222:4222 nats:latest | Out-Null
Start-Sleep -Seconds 2

try {
  # Start subscribers in Docker containers
  $subContainers = @()
  for ($i = 0; $i -lt [int]$Subs; $i++) {
    $containerName = "nats-sub-$i"
    # Start subscriber that counts messages
    docker run -d --name $containerName --network bench-net synadia/nats-box:latest sh -c "nats sub --server nats://nats-bench:4222 bench --count $Msgs | wc -l" | Out-Null
    $subContainers += $containerName
    Start-Sleep -Milliseconds 200
  }

  # Let subscribers settle
  Start-Sleep -Seconds 3

  Write-Host "Starting NATS fanout test: $Msgs msgs × $Subs subs = $([int]$Msgs * [int]$Subs) total deliveries"
  
  # Start timing and publish
  $fanoutSw = [System.Diagnostics.Stopwatch]::StartNew()
  nats bench pub --server nats://127.0.0.1:4222 --msgs $Msgs --size $Size bench | Out-Null
  
  # Wait for all subscribers to complete and collect results
  $subResults = @()
  foreach ($containerName in $subContainers) {
    # Wait for container to exit (subscriber finished)
    $timeout = 30
    $waited = 0
    while ((docker ps -q --filter "name=$containerName").Length -gt 0 -and $waited -lt $timeout) {
      Start-Sleep -Seconds 1
      $waited++
    }
    
    # Get the message count from container logs
    $receivedCount = docker logs $containerName 2>$null | Select-Object -Last 1
    if ([string]::IsNullOrWhiteSpace($receivedCount)) { $receivedCount = "0" }
    
    $subResults += @{
      Container = $containerName
      Received = [int]$receivedCount
    }
    
    # Remove container
    docker rm -f $containerName | Out-Null
  }
  
  $fanoutSw.Stop()
  
  # Calculate results
  $totalReceived = ($subResults | Measure-Object -Property Received -Sum).Sum
  $expectedTotal = ([int]$Msgs) * ([int]$Subs)
  $deliveredThroughput = $totalReceived / $fanoutSw.Elapsed.TotalSeconds
  $deliveredMBps = ($totalReceived * [int]$Size) / $fanoutSw.Elapsed.TotalSeconds / 1048576.0
  
  Write-Host "NATS fanout results:"
  Write-Host "  Delivered throughput: $([Math]::Round($deliveredThroughput, 2)) msgs/sec"
  Write-Host "  Delivered bandwidth: $([Math]::Round($deliveredMBps, 2)) MB/sec"
  Write-Host "  Total messages received: $totalReceived / $expectedTotal"
  Write-Host "  Total time: $([Math]::Round($fanoutSw.Elapsed.TotalSeconds, 2))s"
  
  # Show per-subscriber breakdown
  Write-Host "  Per-subscriber results:"
  foreach ($result in $subResults) {
    Write-Host "    $($result.Container): $($result.Received) msgs"
  }

} finally {
  # Cleanup any remaining containers
  foreach ($containerName in $subContainers) {
    docker rm -f $containerName 2>$null | Out-Null
  }
  docker rm -f nats-bench | Out-Null
}
