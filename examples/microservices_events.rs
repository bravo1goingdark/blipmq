//! Microservices Event-Driven Communication Example
//!
//! This example demonstrates how to use BlipMQ for event-driven communication between microservices.
//! It simulates an e-commerce system with order processing, payment, inventory, and notification services.
//!
//! Architecture:
//! - Order Service: Handles order creation and publishes order events
//! - Payment Service: Processes payments and publishes payment events  
//! - Inventory Service: Manages stock levels and publishes inventory events
//! - Notification Service: Sends notifications based on various events
//! - Event Coordinator: Orchestrates complex workflows
//!
//! Usage:
//!   cargo run --example microservices_events

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{interval, sleep};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Common event types in the system
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type")]
enum Event {
    OrderCreated {
        order_id: String,
        customer_id: String,
        items: Vec<OrderItem>,
        total_amount: f64,
        timestamp: u64,
    },
    PaymentRequested {
        payment_id: String,
        order_id: String,
        amount: f64,
        timestamp: u64,
    },
    PaymentCompleted {
        payment_id: String,
        order_id: String,
        status: PaymentStatus,
        timestamp: u64,
    },
    InventoryReserved {
        reservation_id: String,
        order_id: String,
        items: Vec<OrderItem>,
        timestamp: u64,
    },
    InventoryReleased {
        reservation_id: String,
        order_id: String,
        reason: String,
        timestamp: u64,
    },
    OrderCompleted {
        order_id: String,
        timestamp: u64,
    },
    OrderFailed {
        order_id: String,
        reason: String,
        timestamp: u64,
    },
    NotificationSent {
        notification_id: String,
        recipient: String,
        notification_type: String,
        timestamp: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderItem {
    product_id: String,
    quantity: u32,
    price: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum PaymentStatus {
    Success,
    Failed,
    Pending,
}

/// Order Service - handles order creation and processing
async fn order_service() -> Result<(), Box<dyn std::error::Error>> {
    println!("🛍️ Starting Order Service");
    
    let mut interval = interval(Duration::from_secs(10));
    let mut order_count = 0u32;

    loop {
        interval.tick().await;
        
        // Simulate new order creation
        let order_id = Uuid::new_v4().to_string();
        let customer_id = format!("customer_{}", (order_count % 5) + 1);
        
        let items = vec![
            OrderItem {
                product_id: "prod_001".to_string(),
                quantity: 2,
                price: 29.99,
            },
            OrderItem {
                product_id: "prod_002".to_string(),
                quantity: 1,
                price: 49.99,
            },
        ];
        
        let total_amount: f64 = items.iter().map(|item| item.price * item.quantity as f64).sum();
        
        let event = Event::OrderCreated {
            order_id: order_id.clone(),
            customer_id: customer_id.clone(),
            items,
            total_amount,
            timestamp: current_timestamp(),
        };

        println!("📝 Order Service: Created order {} for {} (${:.2})", 
                order_id, customer_id, total_amount);
        
        // Publish order created event
        publish_event("orders.created", &event).await?;
        
        order_count += 1;
    }
}

/// Payment Service - processes payments
async fn payment_service() -> Result<(), Box<dyn std::error::Error>> {
    println!("💳 Starting Payment Service");
    
    // TODO: In real implementation, subscribe to payment.requested events
    // For simulation, we'll process payments periodically
    let mut interval = interval(Duration::from_secs(15));
    let mut payment_count = 0u32;

    loop {
        interval.tick().await;
        
        // Simulate processing a payment
        let payment_id = Uuid::new_v4().to_string();
        let order_id = format!("order_{}", payment_count);
        
        // 90% success rate
        let status = if payment_count % 10 != 0 {
            PaymentStatus::Success
        } else {
            PaymentStatus::Failed
        };

        let event = Event::PaymentCompleted {
            payment_id: payment_id.clone(),
            order_id: order_id.clone(),
            status: status.clone(),
            timestamp: current_timestamp(),
        };

        match status {
            PaymentStatus::Success => {
                println!("✅ Payment Service: Payment {} succeeded for order {}", 
                        payment_id, order_id);
            },
            PaymentStatus::Failed => {
                println!("❌ Payment Service: Payment {} failed for order {}", 
                        payment_id, order_id);
            },
            _ => {}
        }
        
        publish_event("payments.completed", &event).await?;
        payment_count += 1;
    }
}

/// Inventory Service - manages stock levels
async fn inventory_service() -> Result<(), Box<dyn std::error::Error>> {
    println!("📦 Starting Inventory Service");
    
    let mut stock_levels: HashMap<String, u32> = HashMap::new();
    stock_levels.insert("prod_001".to_string(), 100);
    stock_levels.insert("prod_002".to_string(), 50);
    stock_levels.insert("prod_003".to_string(), 25);
    
    let mut interval = interval(Duration::from_secs(12));
    let mut reservation_count = 0u32;

    loop {
        interval.tick().await;
        
        // Simulate inventory reservation
        let reservation_id = Uuid::new_v4().to_string();
        let order_id = format!("order_{}", reservation_count);
        
        let items = vec![
            OrderItem {
                product_id: "prod_001".to_string(),
                quantity: 2,
                price: 29.99,
            }
        ];
        
        // Check if items are available
        let mut can_reserve = true;
        for item in &items {
            if let Some(current_stock) = stock_levels.get(&item.product_id) {
                if *current_stock < item.quantity {
                    can_reserve = false;
                    break;
                }
            }
        }
        
        if can_reserve {
            // Reserve items
            for item in &items {
                if let Some(stock) = stock_levels.get_mut(&item.product_id) {
                    *stock -= item.quantity;
                }
            }
            
            let event = Event::InventoryReserved {
                reservation_id: reservation_id.clone(),
                order_id: order_id.clone(),
                items,
                timestamp: current_timestamp(),
            };
            
            println!("📋 Inventory Service: Reserved items for order {}", order_id);
            publish_event("inventory.reserved", &event).await?;
        } else {
            let event = Event::InventoryReleased {
                reservation_id: reservation_id.clone(),
                order_id: order_id.clone(),
                reason: "Insufficient stock".to_string(),
                timestamp: current_timestamp(),
            };
            
            println!("⚠️ Inventory Service: Cannot reserve items for order {} - insufficient stock", order_id);
            publish_event("inventory.released", &event).await?;
        }
        
        reservation_count += 1;
    }
}

/// Notification Service - sends notifications
async fn notification_service() -> Result<(), Box<dyn std::error::Error>> {
    println!("📧 Starting Notification Service");
    
    let mut interval = interval(Duration::from_secs(20));
    let mut notification_count = 0u32;

    loop {
        interval.tick().await;
        
        // Simulate sending notifications based on events
        let notification_types = vec![
            "order_confirmation",
            "payment_success", 
            "payment_failed",
            "order_shipped",
            "inventory_alert",
        ];
        
        let notification_type = notification_types[notification_count as usize % notification_types.len()];
        let notification_id = Uuid::new_v4().to_string();
        let recipient = format!("customer_{}@example.com", (notification_count % 5) + 1);
        
        let event = Event::NotificationSent {
            notification_id: notification_id.clone(),
            recipient: recipient.clone(),
            notification_type: notification_type.to_string(),
            timestamp: current_timestamp(),
        };
        
        println!("📨 Notification Service: Sent {} notification to {}", 
                notification_type, recipient);
        
        publish_event("notifications.sent", &event).await?;
        notification_count += 1;
    }
}

/// Event Coordinator - orchestrates complex workflows
async fn event_coordinator() -> Result<(), Box<dyn std::error::Error>> {
    println!("🎯 Starting Event Coordinator");
    
    let mut interval = interval(Duration::from_secs(25));
    let mut coordination_count = 0u32;

    loop {
        interval.tick().await;
        
        // Simulate workflow coordination
        let order_id = format!("order_{}", coordination_count);
        
        // Determine workflow outcome
        let success = coordination_count % 3 != 0;
        
        if success {
            let event = Event::OrderCompleted {
                order_id: order_id.clone(),
                timestamp: current_timestamp(),
            };
            
            println!("🎉 Event Coordinator: Order {} completed successfully", order_id);
            publish_event("orders.completed", &event).await?;
        } else {
            let event = Event::OrderFailed {
                order_id: order_id.clone(),
                reason: "Payment or inventory issue".to_string(),
                timestamp: current_timestamp(),
            };
            
            println!("💥 Event Coordinator: Order {} failed", order_id);
            publish_event("orders.failed", &event).await?;
        }
        
        coordination_count += 1;
    }
}

/// Helper function to publish events
async fn publish_event(topic: &str, event: &Event) -> Result<(), Box<dyn std::error::Error>> {
    let message = serde_json::to_string(event)?;
    
    // TODO: Replace with actual BlipMQ publish call
    // client.publish(topic, message.as_bytes(), Duration::from_secs(60)).await?;
    
    // For demo, just log the event
    println!("   📡 Published to {}: {} bytes", topic, message.len());
    Ok(())
}

fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🚀 BlipMQ Microservices Event-Driven Communication Example");
    println!("===========================================================");
    println!();
    println!("This example demonstrates:");
    println!("• Event-driven microservices architecture");
    println!("• Decoupled service communication via events");
    println!("• Complex workflow orchestration");
    println!("• Async message processing patterns");
    println!();
    
    println!("💡 Event Flow:");
    println!("   Orders → Payments → Inventory → Notifications → Coordination");
    println!("   Each service publishes events that others can subscribe to");
    println!();
    
    println!("📋 Topics in this example:");
    println!("   • orders.created");
    println!("   • orders.completed"); 
    println!("   • orders.failed");
    println!("   • payments.requested");
    println!("   • payments.completed");
    println!("   • inventory.reserved");
    println!("   • inventory.released");
    println!("   • notifications.sent");
    println!();

    // Start all microservices
    let handles = vec![
        tokio::spawn(order_service()),
        tokio::spawn(payment_service()),
        tokio::spawn(inventory_service()),
        tokio::spawn(notification_service()),
        tokio::spawn(event_coordinator()),
    ];

    // Run simulation for 2 minutes
    sleep(Duration::from_secs(120)).await;
    
    println!("\n✅ Simulation complete. Key benefits of this architecture:");
    println!("   • Services are loosely coupled");
    println!("   • Easy to add new services that react to events");
    println!("   • Scalable - each service can scale independently");
    println!("   • Resilient - services can be temporarily unavailable");
    println!("   • BlipMQ provides reliable event delivery");

    // Cancel all tasks
    for handle in handles {
        handle.abort();
    }

    Ok(())
}

// TODO: Add actual BlipMQ integration with event handlers
// 
// Example event handler pattern:
// async fn handle_order_created_event(event: Event) -> Result<(), Box<dyn std::error::Error>> {
//     if let Event::OrderCreated { order_id, total_amount, .. } = event {
//         // Trigger payment request
//         let payment_event = Event::PaymentRequested {
//             payment_id: Uuid::new_v4().to_string(),
//             order_id,
//             amount: total_amount,
//             timestamp: current_timestamp(),
//         };
//         
//         publish_event("payments.requested", &payment_event).await?;
//     }
//     Ok(())
// }