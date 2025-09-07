# MQTT End-to-End Benchmark with Latency Measurement
. "$PSScriptRoot\benchmark-framework.ps1"

function Test-MQTTEndToEnd {
    param($Config)
    
    $result = New-BenchmarkResult -System "MQTT"
    $containers = @("mqtt-broker") + (0..($Config.Subscribers-1) | ForEach-Object { "mqtt-sub-$_" })
    
    try {
        Initialize-BenchmarkNetwork
        Remove-BenchmarkContainers -ContainerNames $containers
        
        # Start MQTT broker (Mosquitto)
        Write-Host "Starting MQTT broker..." -ForegroundColor Yellow
        $scriptsDir = $PSScriptRoot
        docker run -d --name mqtt-broker --network benchmark-net -p 1883:1883 `
            -v "$scriptsDir\mosquitto.conf:/mosquitto/config/mosquitto.conf" `
            eclipse-mosquitto | Out-Null
        Start-Sleep -Seconds 3
        
        # Start subscribers in Docker containers that output timestamped results
        Write-Host "Starting $($Config.Subscribers) MQTT subscribers..." -ForegroundColor Yellow
        $subscriberContainers = @()
        
        for ($i = 0; $i -lt $Config.Subscribers; $i++) {
            $containerName = "mqtt-sub-$i"
            
            # Each subscriber writes received messages with timestamps to a file
            docker run -d --name $containerName --network benchmark-net efrecon/mqtt-client sh -c `
                "mosquitto_sub -h mqtt-broker -p 1883 -t $($Config.Subject) -C $($Config.Messages) > /tmp/messages.log 2>&1; echo 'DONE' >> /tmp/messages.log" | Out-Null
            
            $subscriberContainers += $containerName
            Start-Sleep -Milliseconds 200
        }
        
        # Wait for subscribers to be ready
        Start-Sleep -Seconds 3
        
        # Publish timestamped messages
        Write-Host "Publishing $($Config.Messages) messages..." -ForegroundColor Yellow
        $publishStart = Get-Date
        
        # Create a temporary file with all timestamped messages
        $tempFile = [System.IO.Path]::GetTempFileName()
        
        for ($msgNum = 1; $msgNum -le $Config.Messages; $msgNum++) {
            $message = New-TimestampedMessage -SequenceNumber $msgNum
            Add-Content -Path $tempFile -Value $message -Encoding UTF8
        }
        
        # Publish all messages at once using mosquitto_pub -l
        docker run --rm --network benchmark-net -v "$tempFile":/tmp/messages.txt efrecon/mqtt-client `
            sh -c "mosquitto_pub -h mqtt-broker -p 1883 -t $($Config.Subject) -l < /tmp/messages.txt" | Out-Null
        
        Remove-Item $tempFile -ErrorAction SilentlyContinue
        
        $publishEnd = Get-Date
        $result.PublishTimeSeconds = ($publishEnd - $publishStart).TotalSeconds
        $result.MessagesPublished = $Config.Messages
        
        Write-Host "Waiting for subscribers to complete..." -ForegroundColor Yellow
        
        # Wait for all subscribers to complete (look for DONE marker)
        $maxWaitSeconds = $Config.TimeoutSeconds
        $allCompleted = $false
        $waitStart = Get-Date
        
        while (-not $allCompleted -and ((Get-Date) - $waitStart).TotalSeconds -lt $maxWaitSeconds) {
            $completedCount = 0
            
            foreach ($containerName in $subscriberContainers) {
                $logs = docker logs $containerName 2>$null
                if ($logs -contains "DONE") {
                    $completedCount++
                }
            }
            
            if ($completedCount -eq $subscriberContainers.Count) {
                $allCompleted = $true
                break
            }
            
            Start-Sleep -Seconds 1
        }
        
        $endToEndTime = Get-Date
        $result.EndToEndTimeSeconds = ($endToEndTime - $publishStart).TotalSeconds
        
        # Collect results from each subscriber
        $allLatencies = @()
        $subscriberResults = @()
        $totalReceived = 0
        
        foreach ($i in 0..($subscriberContainers.Count-1)) {
            $containerName = $subscriberContainers[$i]
            $logs = docker logs $containerName 2>$null
            
            # Parse received messages (exclude DONE marker)
            $receivedMessages = $logs | Where-Object { $_ -ne "DONE" -and $_ -match "^\d+\|\d+" }
            $receivedCount = $receivedMessages.Count
            $totalReceived += $receivedCount
            
            # Calculate latencies for this subscriber
            $subLatencies = @()
            $receiveTime = $endToEndTime.AddSeconds(-1)  # Estimate receive time
            
            foreach ($msg in $receivedMessages) {
                $parsed = Parse-TimestampedMessage -Message $msg
                if ($parsed) {
                    $sentTime = [DateTimeOffset]::FromUnixTimeMilliseconds($parsed.TimestampMs)
                    $latencyMs = ($receiveTime - $sentTime.DateTime).TotalMilliseconds
                    if ($latencyMs -gt 0 -and $latencyMs -lt 10000) {  # Sanity check
                        $subLatencies += $latencyMs
                        $allLatencies += $latencyMs
                    }
                }
            }
            
            $avgLatency = if ($subLatencies.Count -gt 0) {
                ($subLatencies | Measure-Object -Average).Average
            } else { 0 }
            
            $subscriberResults += @{
                Id = $i
                Received = $receivedCount
                AvgLatencyMs = [Math]::Round($avgLatency, 2)
            }
            
            # Cleanup container
            docker rm -f $containerName | Out-Null
        }
        
        $result.MessagesDelivered = $totalReceived
        
        if ($result.EndToEndTimeSeconds -gt 0) {
            $result.DeliveredThroughputMsgsPerSec = $result.MessagesDelivered / $result.EndToEndTimeSeconds
            $result.DeliveredBandwidthMBPerSec = ($result.MessagesDelivered * $Config.MessageSize) / $result.EndToEndTimeSeconds / 1048576
        }
        
        # Calculate overall latency statistics
        if ($allLatencies.Count -gt 0) {
            $result.LatencyStats = Get-LatencyStats -LatenciesMs $allLatencies
        }
        
        $result.PerSubscriberResults = $subscriberResults
        $result.Success = $result.MessagesDelivered -gt 0
        
    } catch {
        $result.Success = $false
        $result.ErrorMessage = $_.Exception.Message
        Write-Host "MQTT test failed: $($_.Exception.Message)" -ForegroundColor Red
    } finally {
        # Cleanup
        Remove-BenchmarkContainers -ContainerNames $containers
    }
    
    return $result
}

# Export the test function
if ($MyInvocation.InvocationName -ne '.') {
    # Script is being run directly, not dot-sourced
    $result = Test-MQTTEndToEnd -Config $BenchmarkConfig
    Show-BenchmarkResult -Result $result
}
