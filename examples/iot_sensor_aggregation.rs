//! IoT Sensor Data Aggregation Example
//!
//! This example demonstrates how to use BlipMQ for aggregating sensor data from multiple IoT devices.
//! It shows a common pattern where edge devices publish sensor readings to topics and a central
//! service subscribes to aggregate and process the data.
//!
//! Architecture:
//! - Multiple sensor publishers send data to topic "sensors/{device_id}"
//! - Data aggregator subscribes to "sensors/*" pattern
//! - Processed data is published to "analytics/processed"
//!
//! Usage:
//!   cargo run --example iot_sensor_aggregation

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{interval, sleep};
use serde::{Deserialize, Serialize};

/// Sensor reading data structure
#[derive(Debug, Serialize, Deserialize, Clone)]
struct SensorReading {
    device_id: String,
    sensor_type: String,
    value: f64,
    timestamp: u64,
    location: Option<String>,
}

/// Aggregated sensor data
#[derive(Debug, Serialize, Deserialize)]
struct AggregatedData {
    sensor_type: String,
    avg_value: f64,
    min_value: f64,
    max_value: f64,
    sample_count: usize,
    window_start: u64,
    window_end: u64,
}

/// Simple aggregator that collects sensor data over time windows
struct SensorAggregator {
    readings: HashMap<String, Vec<SensorReading>>,
    window_duration_secs: u64,
}

impl SensorAggregator {
    fn new(window_duration_secs: u64) -> Self {
        Self {
            readings: HashMap::new(),
            window_duration_secs,
        }
    }

    fn add_reading(&mut self, reading: SensorReading) {
        let sensor_type = reading.sensor_type.clone();
        self.readings
            .entry(sensor_type)
            .or_insert_with(Vec::new)
            .push(reading);
    }

    fn aggregate_and_clear(&mut self) -> Vec<AggregatedData> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        let mut aggregated = Vec::new();
        
        for (sensor_type, readings) in self.readings.drain() {
            if readings.is_empty() {
                continue;
            }

            let values: Vec<f64> = readings.iter().map(|r| r.value).collect();
            let avg_value = values.iter().sum::<f64>() / values.len() as f64;
            let min_value = values.iter().cloned().fold(f64::INFINITY, f64::min);
            let max_value = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

            aggregated.push(AggregatedData {
                sensor_type,
                avg_value,
                min_value,
                max_value,
                sample_count: values.len(),
                window_start: now - self.window_duration_secs,
                window_end: now,
            });
        }

        aggregated
    }
}

/// Simulates an IoT sensor device publishing data
async fn sensor_device_simulator(device_id: String, sensor_type: String) -> Result<(), Box<dyn std::error::Error>> {
    // Note: In a real implementation, you would use the BlipMQ client library
    // For this example, we'll show the conceptual code structure
    
    println!("🌡️ Starting sensor device: {} ({})", device_id, sensor_type);
    
    let mut interval = interval(Duration::from_secs(5));
    let mut reading_count = 0u64;

    loop {
        interval.tick().await;
        
        // Generate simulated sensor reading
        let reading = SensorReading {
            device_id: device_id.clone(),
            sensor_type: sensor_type.clone(),
            value: generate_sensor_value(&sensor_type, reading_count),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            location: Some(format!("Building-A-Floor-{}", (reading_count % 5) + 1)),
        };

        // Publish to BlipMQ
        let topic = format!("sensors/{}", device_id);
        let message = serde_json::to_string(&reading)?;
        
        println!("📡 Publishing to {}: {:.2} {}", 
                topic, reading.value, get_unit(&sensor_type));
        
        // TODO: Replace with actual BlipMQ publish call
        // client.publish(&topic, message.as_bytes(), Duration::from_secs(30)).await?;
        
        reading_count += 1;
    }
}

/// Aggregates sensor data and publishes analytics
async fn data_aggregator() -> Result<(), Box<dyn std::error::Error>> {
    println!("📊 Starting data aggregator service");
    
    let mut aggregator = SensorAggregator::new(30); // 30-second windows
    let mut interval = interval(Duration::from_secs(30));

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Process aggregation window
                let aggregated_data = aggregator.aggregate_and_clear();
                
                for data in aggregated_data {
                    println!("📈 Aggregated {}: avg={:.2}, min={:.2}, max={:.2}, samples={}",
                            data.sensor_type, data.avg_value, data.min_value, 
                            data.max_value, data.sample_count);
                    
                    // Publish aggregated data
                    let message = serde_json::to_string(&data)?;
                    
                    // TODO: Replace with actual BlipMQ publish call
                    // client.publish("analytics/processed", message.as_bytes(), Duration::from_secs(300)).await?;
                }
            }
            
            // TODO: Replace with actual BlipMQ subscription
            // received_message = client.subscribe("sensors/*").await => {
            //     if let Ok(reading) = serde_json::from_slice::<SensorReading>(&received_message.payload) {
            //         aggregator.add_reading(reading);
            //     }
            // }
        }
    }
}

fn generate_sensor_value(sensor_type: &str, count: u64) -> f64 {
    use std::f64::consts::PI;
    
    let base_variation = (count as f64 * 0.1).sin() * 0.5;
    let noise = (count as f64 * 1.7).sin() * 0.1;
    
    match sensor_type {
        "temperature" => 22.0 + base_variation + noise,
        "humidity" => 45.0 + base_variation * 10.0 + noise * 5.0,
        "pressure" => 1013.25 + base_variation * 5.0 + noise * 2.0,
        "light" => 500.0 + base_variation * 200.0 + noise * 50.0,
        _ => count as f64 % 100.0,
    }
}

fn get_unit(sensor_type: &str) -> &'static str {
    match sensor_type {
        "temperature" => "°C",
        "humidity" => "%",
        "pressure" => "hPa",
        "light" => "lux",
        _ => "units",
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🚀 BlipMQ IoT Sensor Aggregation Example");
    println!("==========================================");
    println!();
    println!("This example demonstrates:");
    println!("• Multiple IoT devices publishing sensor data");
    println!("• Real-time data aggregation and analytics");
    println!("• Pub/Sub patterns for scalable IoT architecture");
    println!();
    
    // Note: In a real implementation, you would:
    // 1. Start a BlipMQ broker or connect to existing one
    // 2. Create BlipMQ client connections for each component
    // 3. Set up proper error handling and reconnection logic
    
    println!("💡 To run this with actual BlipMQ:");
    println!("   1. Start BlipMQ broker: cargo run --bin blipmq -- start");
    println!("   2. Implement BlipMQ client integration");
    println!("   3. Use proper topic patterns and message TTLs");
    println!();

    // Simulate the architecture
    let handles = vec![
        tokio::spawn(sensor_device_simulator("temp_001".to_string(), "temperature".to_string())),
        tokio::spawn(sensor_device_simulator("temp_002".to_string(), "temperature".to_string())),
        tokio::spawn(sensor_device_simulator("humid_001".to_string(), "humidity".to_string())),
        tokio::spawn(sensor_device_simulator("press_001".to_string(), "pressure".to_string())),
        tokio::spawn(data_aggregator()),
    ];

    // Run simulation for 2 minutes
    sleep(Duration::from_secs(120)).await;
    
    println!("\n✅ Simulation complete. In a real deployment:");
    println!("   • Sensors would run on edge devices");
    println!("   • Aggregator would be a cloud service");
    println!("   • BlipMQ provides the messaging backbone");
    println!("   • Data could trigger alerts, dashboards, etc.");

    // Cancel all tasks
    for handle in handles {
        handle.abort();
    }

    Ok(())
}

// TODO: Add actual BlipMQ client integration
// use blipmq::Client;
//
// async fn create_client() -> Result<Client, blipmq::Error> {
//     Client::connect("127.0.0.1:7878").await
// }