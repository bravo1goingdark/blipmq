# Fair End-to-End Message Queue Benchmarking Framework
# Measures delivered throughput and latency consistently across all systems

param(
    [int]$Messages = 5000,
    [int]$MessageSize = 100,
    [int]$Subscribers = 8,
    [int]$WarmupMessages = 100
)

# Common benchmark configuration
$script:BenchmarkConfig = @{
    Messages = $Messages
    MessageSize = $MessageSize
    Subscribers = $Subscribers
    WarmupMessages = $WarmupMessages
    Subject = "benchmark_topic"
    TimeoutSeconds = 60
}

# Result structure for consistent reporting
function New-BenchmarkResult {
    param(
        [string]$System,
        [bool]$Success = $false
    )
    
    return @{
        System = $System
        Success = $Success
        MessagesPublished = 0
        MessagesDelivered = 0
        ExpectedDeliveries = $script:BenchmarkConfig.Messages * $script:BenchmarkConfig.Subscribers
        DeliveredThroughputMsgsPerSec = 0
        DeliveredBandwidthMBPerSec = 0
        PublishTimeSeconds = 0
        EndToEndTimeSeconds = 0
        LatencyStats = @{
            MinMs = 0
            MaxMs = 0
            MeanMs = 0
            P50Ms = 0
            P95Ms = 0
            P99Ms = 0
        }
        PerSubscriberResults = @()
        ErrorMessage = ""
    }
}

# Generate timestamped message payload
function New-TimestampedMessage {
    param([int]$SequenceNumber)
    
    $timestamp = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    $payload = "$SequenceNumber|$timestamp"
    
    # Pad to desired size with 'x' characters
    $targetSize = $script:BenchmarkConfig.MessageSize
    $currentSize = [System.Text.Encoding]::UTF8.GetByteCount($payload)
    
    if ($currentSize -lt $targetSize) {
        $paddingNeeded = $targetSize - $currentSize
        $payload += ('x' * $paddingNeeded)
    }
    
    return $payload
}

# Parse message to extract timestamp and sequence
function Parse-TimestampedMessage {
    param([string]$Message)
    
    if ($Message -match '^(\d+)\|(\d+)') {
        return @{
            Sequence = [int]$matches[1]
            TimestampMs = [long]$matches[2]
        }
    }
    
    return $null
}

# Calculate latency statistics from array of latencies (in milliseconds)
function Get-LatencyStats {
    param([array]$LatenciesMs)
    
    if ($LatenciesMs.Count -eq 0) {
        return @{
            MinMs = 0; MaxMs = 0; MeanMs = 0
            P50Ms = 0; P95Ms = 0; P99Ms = 0
        }
    }
    
    $sorted = $LatenciesMs | Sort-Object
    $count = $sorted.Count
    
    return @{
        MinMs = [Math]::Round($sorted[0], 2)
        MaxMs = [Math]::Round($sorted[-1], 2)
        MeanMs = [Math]::Round(($sorted | Measure-Object -Average).Average, 2)
        P50Ms = [Math]::Round($sorted[[Math]::Floor($count * 0.50)], 2)
        P95Ms = [Math]::Round($sorted[[Math]::Floor($count * 0.95)], 2)
        P99Ms = [Math]::Round($sorted[[Math]::Floor($count * 0.99)], 2)
    }
}

# Docker network management
function Initialize-BenchmarkNetwork {
    try {
        docker network create benchmark-net 2>$null | Out-Null
    } catch {
        # Network already exists, that's fine
    }
}

function Remove-BenchmarkContainers {
    param([string[]]$ContainerNames)
    
    foreach ($name in $ContainerNames) {
        try {
            docker rm -f $name 2>$null | Out-Null
        } catch {
            # Container doesn't exist, that's fine
        }
    }
}

# Display results in a consistent format
function Show-BenchmarkResult {
    param($Result)
    
    Write-Host "`n=== $($Result.System) Results ===" -ForegroundColor Cyan
    
    if (-not $Result.Success) {
        Write-Host "❌ Test Failed: $($Result.ErrorMessage)" -ForegroundColor Red
        return
    }
    
    Write-Host "✅ Test Successful" -ForegroundColor Green
    Write-Host "📊 Messages: $($Result.MessagesDelivered) / $($Result.ExpectedDeliveries) delivered"
    Write-Host "🚀 Delivered Throughput: $([Math]::Round($Result.DeliveredThroughputMsgsPerSec, 0)) msgs/sec"
    Write-Host "💾 Delivered Bandwidth: $([Math]::Round($Result.DeliveredBandwidthMBPerSec, 2)) MB/sec"
    Write-Host "⏱️  Publish Time: $([Math]::Round($Result.PublishTimeSeconds, 2))s"
    Write-Host "🔄 End-to-End Time: $([Math]::Round($Result.EndToEndTimeSeconds, 2))s"
    
    $latency = $Result.LatencyStats
    Write-Host "📈 Latency Stats:"
    Write-Host "   Min: $($latency.MinMs)ms | Max: $($latency.MaxMs)ms | Mean: $($latency.MeanMs)ms"
    Write-Host "   P50: $($latency.P50Ms)ms | P95: $($latency.P95Ms)ms | P99: $($latency.P99Ms)ms"
    
    if ($Result.PerSubscriberResults.Count -gt 0) {
        Write-Host "👥 Per-Subscriber Results:"
        foreach ($sub in $Result.PerSubscriberResults) {
            Write-Host "   Sub $($sub.Id): $($sub.Received) msgs, avg latency $($sub.AvgLatencyMs)ms"
        }
    }
}

# Export the functions and config for use by individual benchmarks
Export-ModuleMember -Function * -Variable BenchmarkConfig
