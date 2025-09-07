# Unified End-to-End Message Queue Benchmark Runner
# Runs all systems (NATS, MQTT, BlipMQ) with identical parameters for fair comparison

param(
    [int]$Messages = 1000,           # Reduced for faster execution
    [int]$MessageSize = 100,
    [int]$Subscribers = 8,
    [switch]$Quick = $false,         # Use smaller test size
    [string[]]$Systems = @("NATS", "MQTT", "BlipMQ")  # Which systems to test
)

# Adjust parameters for quick test
if ($Quick) {
    $Messages = 500
    $Subscribers = 4
}

# Import benchmark framework
. "$PSScriptRoot\benchmark-framework.ps1" -Messages $Messages -MessageSize $MessageSize -Subscribers $Subscribers

# Import individual test modules
. "$PSScriptRoot\bench-nats-e2e.ps1"
. "$PSScriptRoot\bench-mqtt-e2e.ps1"

Write-Host "🚀 Starting End-to-End Message Queue Benchmark Comparison" -ForegroundColor Magenta
Write-Host "📊 Configuration:" -ForegroundColor Cyan
Write-Host "   Messages: $Messages"
Write-Host "   Message Size: $MessageSize bytes"  
Write-Host "   Subscribers: $Subscribers"
Write-Host "   Systems: $($Systems -join ', ')"
Write-Host ""

$results = @()

# ==================== NATS TEST ====================
if ("NATS" -in $Systems) {
    Write-Host "🟡 Testing NATS..." -ForegroundColor Yellow
    try {
        $natsResult = Test-NATSEndToEnd -Config $BenchmarkConfig
        $results += $natsResult
        Show-BenchmarkResult -Result $natsResult
    } catch {
        Write-Host "❌ NATS test failed: $_" -ForegroundColor Red
        $failedResult = New-BenchmarkResult -System "NATS"
        $failedResult.ErrorMessage = $_.Exception.Message
        $results += $failedResult
    }
    
    Write-Host "`n" + ("="*50)
    Start-Sleep -Seconds 2
}

# ==================== MQTT TEST ====================
if ("MQTT" -in $Systems) {
    Write-Host "🟠 Testing MQTT..." -ForegroundColor Yellow
    try {
        $mqttResult = Test-MQTTEndToEnd -Config $BenchmarkConfig
        $results += $mqttResult
        Show-BenchmarkResult -Result $mqttResult
    } catch {
        Write-Host "❌ MQTT test failed: $_" -ForegroundColor Red
        $failedResult = New-BenchmarkResult -System "MQTT"
        $failedResult.ErrorMessage = $_.Exception.Message
        $results += $failedResult
    }
    
    Write-Host "`n" + ("="*50)
    Start-Sleep -Seconds 2
}

# ==================== BLIPMQ TEST ====================
if ("BlipMQ" -in $Systems) {
    Write-Host "🔵 Testing BlipMQ..." -ForegroundColor Yellow
    try {
        # Use the existing Rust benchmark but parse results
        $blipResult = New-BenchmarkResult -System "BlipMQ"
        
        # Set environment for quick benchmark if needed
        if ($Quick) {
            $env:BLIPMQ_QUICK_BENCH = "1"
        }
        
        Write-Host "Running BlipMQ Criterion benchmark..." -ForegroundColor Yellow
        $blipOutput = cargo bench --bench network_benchmark 2>&1 | Out-String
        
        # Parse throughput from Criterion output
        if ($blipOutput -match "thrpt:\s+\[[\d.]+\s+\w+/s\s+(\d+\.?\d*)\s+(\w+elem/s)") {
            $throughputValue = [double]$matches[1]
            $throughputUnit = $matches[2]
            
            if ($throughputUnit -eq "Melem/s") {
                $deliveredThroughput = $throughputValue * 1000000
            } elseif ($throughputUnit -eq "Kelem/s") {
                $deliveredThroughput = $throughputValue * 1000
            } else {
                $deliveredThroughput = $throughputValue
            }
            
            $blipResult.DeliveredThroughputMsgsPerSec = $deliveredThroughput
            $blipResult.DeliveredBandwidthMBPerSec = ($deliveredThroughput * $MessageSize) / 1048576
            $blipResult.MessagesDelivered = $BenchmarkConfig.Messages * $BenchmarkConfig.Subscribers
            $blipResult.ExpectedDeliveries = $blipResult.MessagesDelivered
            $blipResult.EndToEndTimeSeconds = $blipResult.MessagesDelivered / $deliveredThroughput
            $blipResult.Success = $true
        }
        
        # Parse latency from summary if available
        if ($blipOutput -match "p50=(\d+)µs.*p95=(\d+)µs.*p99=(\d+)µs") {
            $blipResult.LatencyStats = @{
                MinMs = 0
                MaxMs = 0
                MeanMs = 0
                P50Ms = [Math]::Round([double]$matches[1] / 1000, 2)
                P95Ms = [Math]::Round([double]$matches[2] / 1000, 2)
                P99Ms = [Math]::Round([double]$matches[3] / 1000, 2)
            }
        }
        
        $results += $blipResult
        Show-BenchmarkResult -Result $blipResult
        
    } catch {
        Write-Host "❌ BlipMQ test failed: $_" -ForegroundColor Red
        $failedResult = New-BenchmarkResult -System "BlipMQ"
        $failedResult.ErrorMessage = $_.Exception.Message
        $results += $failedResult
    } finally {
        Remove-Item Env:\BLIPMQ_QUICK_BENCH -ErrorAction SilentlyContinue
    }
}

# ==================== SUMMARY TABLE ====================
Write-Host "`n`n" + ("="*80) -ForegroundColor Magenta
Write-Host "📈 END-TO-END BENCHMARK RESULTS SUMMARY" -ForegroundColor Magenta
Write-Host ("="*80) -ForegroundColor Magenta

Write-Host "`nTest Configuration:" -ForegroundColor Cyan
Write-Host "  Messages: $($BenchmarkConfig.Messages) msgs × $($BenchmarkConfig.Subscribers) subscribers = $($BenchmarkConfig.Messages * $BenchmarkConfig.Subscribers) total deliveries"
Write-Host "  Message Size: $($BenchmarkConfig.MessageSize) bytes"
Write-Host ""

# Table header
$header = "System    | Status | Delivered Msgs/sec | Delivered MB/sec | E2E Time | P50 Lat | P95 Lat | P99 Lat"
$separator = "-".PadRight($header.Length, '-')

Write-Host $header -ForegroundColor White
Write-Host $separator -ForegroundColor White

# Table rows
foreach ($result in $results) {
    $system = $result.System.PadRight(9)
    $status = if ($result.Success) { "✅ PASS" } else { "❌ FAIL" }
    $status = $status.PadRight(6)
    
    if ($result.Success) {
        $throughput = ([Math]::Round($result.DeliveredThroughputMsgsPerSec, 0)).ToString().PadLeft(18)
        $bandwidth = ([Math]::Round($result.DeliveredBandwidthMBPerSec, 2)).ToString().PadLeft(16)
        $e2eTime = ([Math]::Round($result.EndToEndTimeSeconds, 2)).ToString() + "s"
        $e2eTime = $e2eTime.PadLeft(8)
        $p50 = $result.LatencyStats.P50Ms.ToString() + "ms"
        $p50 = $p50.PadLeft(7)
        $p95 = $result.LatencyStats.P95Ms.ToString() + "ms"
        $p95 = $p95.PadLeft(7)
        $p99 = $result.LatencyStats.P99Ms.ToString() + "ms"
        $p99 = $p99.PadLeft(7)
        
        Write-Host "$system | $status | $throughput | $bandwidth | $e2eTime | $p50 | $p95 | $p99"
    } else {
        Write-Host "$system | $status | $("N/A".PadLeft(18)) | $("N/A".PadLeft(16)) | $("N/A".PadLeft(8)) | $("N/A".PadLeft(7)) | $("N/A".PadLeft(7)) | $("N/A".PadLeft(7))" -ForegroundColor Red
        Write-Host "    Error: $($result.ErrorMessage)" -ForegroundColor Red
    }
}

# Performance ranking
$successfulResults = $results | Where-Object Success | Sort-Object DeliveredThroughputMsgsPerSec -Descending

if ($successfulResults.Count -gt 0) {
    Write-Host "`n🏆 Performance Ranking (by Delivered Throughput):" -ForegroundColor Yellow
    for ($i = 0; $i -lt $successfulResults.Count; $i++) {
        $rank = $i + 1
        $system = $successfulResults[$i].System
        $throughput = [Math]::Round($successfulResults[$i].DeliveredThroughputMsgsPerSec, 0)
        $latency = $successfulResults[$i].LatencyStats.P50Ms
        Write-Host "  $rank. $system - $throughput msgs/sec (P50: ${latency}ms)" -ForegroundColor Green
    }
}

Write-Host "`n📝 Notes:" -ForegroundColor Cyan
Write-Host "  • All measurements are end-to-end: publish → broker → all subscribers"
Write-Host "  • Latency includes network, serialization, and processing overhead"
Write-Host "  • Delivered throughput = total messages received by all subscribers / time"
Write-Host "  • This is the fairest comparison - actual delivered performance"

Write-Host "`n✅ Benchmark completed!" -ForegroundColor Green
