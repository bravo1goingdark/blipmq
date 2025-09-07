# BlipMQ benchmark helpers (Windows PowerShell)
param(
  [switch]$Quick = $false
)

$ErrorActionPreference = "Stop"

if ($Quick) { $env:BLIPMQ_QUICK_BENCH = "1" } else { Remove-Item Env:\BLIPMQ_QUICK_BENCH -ErrorAction SilentlyContinue }

# Build release with mimalloc
cargo build --release --features mimalloc
if ($LASTEXITCODE -ne 0) { throw "build failed" }

# Run benchmark
cargo bench --bench network_benchmark

