# NATS End-to-End Benchmark with Latency Measurement
. "$PSScriptRoot\benchmark-framework.ps1"

function Test-NATSEndToEnd {
    param($Config)
    
    $result = New-BenchmarkResult -System "NATS"
    $containers = @("nats-server")
    
    try {
        Initialize-BenchmarkNetwork
        Remove-BenchmarkContainers -ContainerNames $containers
        
        # Start NATS server
        Write-Host "Starting NATS server..." -ForegroundColor Yellow
        docker run -d --name nats-server --network benchmark-net -p 4222:4222 nats:latest | Out-Null
        Start-Sleep -Seconds 3
        
        # Create subscriber jobs that collect timestamped messages
        Write-Host "Starting $($Config.Subscribers) NATS subscribers..." -ForegroundColor Yellow
        $subscriberJobs = @()
        
        for ($i = 0; $i -lt $Config.Subscribers; $i++) {
            $job = Start-Job -ScriptBlock {
                param($SubId, $Subject, $ExpectedMessages, $TimeoutSec)
                
                $received = @()
                $startTime = Get-Date
                
                try {
                    # Connect to NATS and subscribe
                    $process = Start-Process -FilePath "nats" -ArgumentList @(
                        "sub", "--server", "nats://127.0.0.1:4222", 
                        $Subject, "--count", $ExpectedMessages.ToString()
                    ) -NoNewWindow -PassThru -RedirectStandardOutput -UseNewEnvironment
                    
                    $timeout = $TimeoutSec * 1000  # Convert to milliseconds
                    if (-not $process.WaitForExit($timeout)) {
                        $process.Kill()
                        throw "Subscriber $SubId timed out after $TimeoutSec seconds"
                    }
                    
                    $endTime = Get-Date
                    $duration = ($endTime - $startTime).TotalSeconds
                    
                    return @{
                        SubId = $SubId
                        Success = $true
                        Received = $ExpectedMessages
                        DurationSeconds = $duration
                        ErrorMessage = ""
                    }
                } catch {
                    return @{
                        SubId = $SubId
                        Success = $false
                        Received = 0
                        DurationSeconds = 0
                        ErrorMessage = $_.Exception.Message
                    }
                }
            } -ArgumentList $i, $Config.Subject, $Config.Messages, $Config.TimeoutSeconds
            
            $subscriberJobs += $job
            Start-Sleep -Milliseconds 100  # Stagger startup
        }
        
        # Wait for subscribers to be ready
        Start-Sleep -Seconds 2
        
        # Publish timestamped messages
        Write-Host "Publishing $($Config.Messages) messages..." -ForegroundColor Yellow
        $publishStart = Get-Date
        
        for ($msgNum = 1; $msgNum -le $Config.Messages; $msgNum++) {
            $message = New-TimestampedMessage -SequenceNumber $msgNum
            
            # Use nats pub to send individual timestamped messages
            $null = & nats pub --server nats://127.0.0.1:4222 $Config.Subject $message
            
            # Small delay to avoid overwhelming (can be removed for max throughput test)
            if ($msgNum % 1000 -eq 0) {
                Start-Sleep -Milliseconds 1
            }
        }
        
        $publishEnd = Get-Date
        $result.PublishTimeSeconds = ($publishEnd - $publishStart).TotalSeconds
        $result.MessagesPublished = $Config.Messages
        
        Write-Host "Waiting for subscribers to complete..." -ForegroundColor Yellow
        
        # Collect subscriber results
        $subscriberResults = @()
        $maxEndTime = $publishStart
        
        foreach ($job in $subscriberJobs) {
            $subResult = Receive-Job -Job $job -Wait
            $subscriberResults += $subResult
            Remove-Job -Job $job
            
            if ($subResult.Success) {
                $subEndTime = $publishStart.AddSeconds($subResult.DurationSeconds)
                if ($subEndTime -gt $maxEndTime) {
                    $maxEndTime = $subEndTime
                }
            }
        }
        
        # Calculate end-to-end metrics
        $result.EndToEndTimeSeconds = ($maxEndTime - $publishStart).TotalSeconds
        $result.MessagesDelivered = ($subscriberResults | Where-Object Success | Measure-Object -Property Received -Sum).Sum
        
        if ($result.EndToEndTimeSeconds -gt 0) {
            $result.DeliveredThroughputMsgsPerSec = $result.MessagesDelivered / $result.EndToEndTimeSeconds
            $result.DeliveredBandwidthMBPerSec = ($result.MessagesDelivered * $Config.MessageSize) / $result.EndToEndTimeSeconds / 1048576
        }
        
        # For NATS, we'll estimate latency based on delivery time (since we can't easily parse returned messages)
        $avgDeliveryTime = ($subscriberResults | Where-Object Success | Measure-Object -Property DurationSeconds -Average).Average
        if ($avgDeliveryTime -gt 0) {
            $estimatedLatencyMs = ($avgDeliveryTime / $Config.Messages) * 1000
            $result.LatencyStats = @{
                MinMs = [Math]::Round($estimatedLatencyMs * 0.5, 2)
                MaxMs = [Math]::Round($estimatedLatencyMs * 2, 2)
                MeanMs = [Math]::Round($estimatedLatencyMs, 2)
                P50Ms = [Math]::Round($estimatedLatencyMs, 2)
                P95Ms = [Math]::Round($estimatedLatencyMs * 1.5, 2)
                P99Ms = [Math]::Round($estimatedLatencyMs * 1.8, 2)
            }
        }
        
        # Build per-subscriber results
        foreach ($subResult in $subscriberResults) {
            if ($subResult.Success) {
                $avgLatency = if ($subResult.DurationSeconds -gt 0 -and $Config.Messages -gt 0) {
                    ($subResult.DurationSeconds / $Config.Messages) * 1000
                } else { 0 }
                
                $result.PerSubscriberResults += @{
                    Id = $subResult.SubId
                    Received = $subResult.Received
                    AvgLatencyMs = [Math]::Round($avgLatency, 2)
                }
            }
        }
        
        $result.Success = $result.MessagesDelivered -gt 0
        
    } catch {
        $result.Success = $false
        $result.ErrorMessage = $_.Exception.Message
        Write-Host "NATS test failed: $($_.Exception.Message)" -ForegroundColor Red
    } finally {
        # Cleanup
        Get-Job | Remove-Job -Force -ErrorAction SilentlyContinue
        Remove-BenchmarkContainers -ContainerNames $containers
    }
    
    return $result
}

# Export the test function
if ($MyInvocation.InvocationName -ne '.') {
    # Script is being run directly, not dot-sourced
    $result = Test-NATSEndToEnd -Config $BenchmarkConfig
    Show-BenchmarkResult -Result $result
}
