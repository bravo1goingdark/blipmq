# Benchmark NATS locally using Docker-only (server + CLI)
param(
  [string]$Msgs = "200000",
  [string]$Size = "100",
  [string]$Subs = "64"  # kept for compatibility, not used in Docker-only pub test
)

$ErrorActionPreference = "Stop"

# Ensure a dedicated docker network so the CLI container can resolve the broker by name
try { docker network create bench-net | Out-Null } catch { }

# Start NATS server (ephemeral)
docker run --rm -d --name nats-bench --network bench-net -p 4222:4222 nats:latest | Out-Null
Start-Sleep -Seconds 2

try {
  # Use the host-installed NATS CLI for the benchmark (server remains in Docker).
  # Simple publisher-only benchmark against subject 'bench'.
  nats bench pub --server nats://127.0.0.1:4222 --msgs $Msgs --size $Size bench
} finally {
  docker rm -f nats-bench | Out-Null
}
