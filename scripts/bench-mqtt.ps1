# Benchmark MQTT (Mosquitto) using Docker-only clients (no host CLI deps)
param(
  [string]$Msgs = "200000",
  [string]$Size = "100",
  [string]$Subs = "64"  # kept for compatibility, not used in this simple pub test
)

$ErrorActionPreference = "Stop"

# Ensure a dedicated docker network so client container can resolve broker by name
try { docker network create bench-net | Out-Null } catch { }

# Start Mosquitto broker with minimal config allowing anonymous access
$scriptsDir = "C:\Users\Ashutosh Kumar\RustroverProjects\blipmq\scripts"
$configMount = "${scriptsDir}:\mosquitto\config"
docker run --rm -d --name mosq-bench --network bench-net -p 1883:1883 -v "$scriptsDir\mosquitto.conf:/mosquitto/config/mosquitto.conf" eclipse-mosquitto | Out-Null
Start-Sleep -Seconds 5

try {
  # Use efrecon/mqtt-client which bundles mosquitto_pub and a minimal shell.
  # Generate $Msgs messages, each of size ~$Size bytes, and pipe via -l.
  # Precompute payload in PowerShell to avoid complex shell quoting inside the container.
  $payload = ('x' * [int]$Size)
  $sw = [System.Diagnostics.Stopwatch]::StartNew()
  docker run --rm --network bench-net -e MSGS=$Msgs -e PAYLOAD="$payload" efrecon/mqtt-client sh -lc 'yes "$PAYLOAD" | head -n "$MSGS" | mosquitto_pub -l -h mosq-bench -p 1883 -t bench -q 0'
  $sw.Stop()
  $secs = [Math]::Max(0.000001, $sw.Elapsed.TotalSeconds)
  $mps = [Math]::Round(([double]$Msgs) / $secs, 2)
  $mbps = [Math]::Round((( [double]$Msgs * [double]$Size) / $secs) / 1048576.0, 2)
  Write-Host "MQTT pub stats: $mps msgs/sec ~ $mbps MB/sec"
} finally {
  docker rm -f mosq-bench | Out-Null
}
