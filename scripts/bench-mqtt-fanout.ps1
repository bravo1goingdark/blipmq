# MQTT fanout benchmark: 8 subscribers measuring delivered throughput
param(
  [string]$Msgs = "2000",
  [string]$Size = "100",
  [string]$Subs = "8"
)

$ErrorActionPreference = "Stop"

# Ensure a dedicated docker network
try { docker network create bench-net | Out-Null } catch { }

# Start Mosquitto broker with config allowing anonymous access
$scriptsDir = "C:\Users\Ashutosh Kumar\RustroverProjects\blipmq\scripts"
docker run --rm -d --name mosq-bench --network bench-net -p 1883:1883 -v "$scriptsDir\mosquitto.conf:/mosquitto/config/mosquitto.conf" eclipse-mosquitto | Out-Null
Start-Sleep -Seconds 3

try {
  # Start subscribers in background Docker containers
  $subContainers = @()
  for ($i = 0; $i -lt [int]$Subs; $i++) {
    $containerName = "mqtt-sub-$i"
    # Start subscriber that counts messages and outputs count when done
    docker run -d --name $containerName --network bench-net efrecon/mqtt-client sh -c "mosquitto_sub -h mosq-bench -p 1883 -t bench -c $Msgs | wc -l" | Out-Null
    $subContainers += $containerName
    Start-Sleep -Milliseconds 200
  }

  # Let subscribers settle
  Start-Sleep -Seconds 2

  Write-Host "Starting MQTT fanout test: $Msgs msgs × $Subs subs = $([int]$Msgs * [int]$Subs) total deliveries"
  
  # Generate payload and start timing
  $payload = ('x' * [int]$Size)
  $fanoutSw = [System.Diagnostics.Stopwatch]::StartNew()
  
  # Publish messages
  docker run --rm --network bench-net -e MSGS=$Msgs -e PAYLOAD="$payload" efrecon/mqtt-client sh -lc 'yes "$PAYLOAD" | head -n "$MSGS" | mosquitto_pub -l -h mosq-bench -p 1883 -t bench -q 0' | Out-Null
  
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
  
  Write-Host "MQTT fanout results:"
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
  docker rm -f mosq-bench | Out-Null
}
