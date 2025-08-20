//! Graceful shutdown handling for production deployments.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;
use tokio::sync::{broadcast, RwLock, Semaphore};
use tracing::{info, warn, error};

/// Shutdown coordinator for graceful termination
#[derive(Debug)]
pub struct ShutdownCoordinator {
    shutdown_signal: broadcast::Sender<()>,
    active_tasks: Arc<Semaphore>,
    is_shutting_down: Arc<AtomicBool>,
    shutdown_timeout: Duration,
    components: Arc<RwLock<Vec<ShutdownComponent>>>,
}

#[derive(Debug)]
pub struct ShutdownComponent {
    pub name: String,
    pub priority: u8, // Lower number = higher priority (shutdown first)
    pub shutdown_fn: Box<dyn Fn() -> tokio::sync::oneshot::Receiver<Result<(), String>> + Send + Sync>,
}

impl ShutdownCoordinator {
    pub fn new(shutdown_timeout: Duration) -> Self {
        let (shutdown_tx, _) = broadcast::channel(16);
        
        Self {
            shutdown_signal: shutdown_tx,
            active_tasks: Arc::new(Semaphore::new(0)),
            is_shutting_down: Arc::new(AtomicBool::new(false)),
            shutdown_timeout,
            components: Arc::new(RwLock::new(Vec::new())),
        }
    }
    
    /// Register a component for graceful shutdown
    pub async fn register_component<F>(&self, name: String, priority: u8, shutdown_fn: F)
    where
        F: Fn() -> tokio::sync::oneshot::Receiver<Result<(), String>> + Send + Sync + 'static,
    {
        let component = ShutdownComponent {
            name,
            priority,
            shutdown_fn: Box::new(shutdown_fn),
        };
        
        let mut components = self.components.write().await;
        components.push(component);
        
        // Sort by priority (lower number = higher priority)
        components.sort_by_key(|c| c.priority);
    }
    
    /// Start monitoring shutdown signals
    pub async fn monitor_signals(&self) {
        let shutdown_signal = self.shutdown_signal.clone();
        let is_shutting_down = Arc::clone(&self.is_shutting_down);
        
        tokio::spawn(async move {
            let ctrl_c = signal::ctrl_c();
            
            #[cfg(unix)]
            let terminate = async {
                signal::unix::signal(signal::unix::SignalKind::terminate())
                    .expect("Failed to install SIGTERM handler")
                    .recv()
                    .await;
            };
            
            #[cfg(not(unix))]
            let terminate = std::future::pending::<()>();
            
            tokio::select! {
                _ = ctrl_c => {
                    info!("Received SIGINT (Ctrl+C)");
                }
                _ = terminate => {
                    info!("Received SIGTERM");
                }
            }
            
            is_shutting_down.store(true, Ordering::SeqCst);
            let _ = shutdown_signal.send(());
        });
    }
    
    /// Check if shutdown is in progress
    pub fn is_shutting_down(&self) -> bool {
        self.is_shutting_down.load(Ordering::Relaxed)
    }
    
    /// Get shutdown signal receiver
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.shutdown_signal.subscribe()
    }
    
    /// Track an active task
    pub fn track_task(&self) -> Option<TaskTracker> {
        if self.is_shutting_down() {
            return None;
        }
        
        // Try to acquire a permit for the task
        if let Ok(permit) = self.active_tasks.try_acquire() {
            Some(TaskTracker {
                _permit: permit,
            })
        } else {
            None
        }
    }
    
    /// Initiate graceful shutdown
    pub async fn shutdown(&self) -> Result<(), String> {
        info!("🛑 Initiating graceful shutdown...");
        
        self.is_shutting_down.store(true, Ordering::SeqCst);
        let _ = self.shutdown_signal.send(());
        
        // Shutdown components in priority order
        let components = self.components.read().await;
        for component in components.iter() {
            info!("Shutting down component: {}", component.name);
            
            let shutdown_rx = (component.shutdown_fn)();
            
            match tokio::time::timeout(self.shutdown_timeout, shutdown_rx).await {
                Ok(Ok(Ok(()))) => {
                    info!("✅ Component '{}' shut down successfully", component.name);
                }
                Ok(Ok(Err(e))) => {
                    warn!("⚠️  Component '{}' shutdown with error: {}", component.name, e);
                }
                Ok(Err(_)) => {
                    warn!("⚠️  Component '{}' shutdown channel closed", component.name);
                }
                Err(_) => {
                    error!("❌ Component '{}' shutdown timed out", component.name);
                }
            }
        }
        
        // Wait for active tasks to complete
        info!("Waiting for active tasks to complete...");
        let start_time = std::time::Instant::now();
        
        while start_time.elapsed() < self.shutdown_timeout {
            if self.active_tasks.available_permits() == 0 {
                info!("✅ All tasks completed");
                break;
            }
            
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        
        if self.active_tasks.available_permits() > 0 {
            warn!("⚠️  Some tasks did not complete within timeout");
        }
        
        info!("🏁 Graceful shutdown completed");
        Ok(())
    }
    
    /// Force immediate shutdown (emergency)
    pub async fn force_shutdown(&self) {
        error!("🚨 Force shutdown initiated!");
        self.is_shutting_down.store(true, Ordering::SeqCst);
        let _ = self.shutdown_signal.send(());
        
        // Give a brief moment for cleanup, then exit
        tokio::time::sleep(Duration::from_secs(1)).await;
        std::process::exit(1);
    }
}

/// RAII task tracker that automatically decrements task count when dropped
pub struct TaskTracker {
    _permit: tokio::sync::SemaphorePermit<'static>,
}

/// Graceful shutdown utilities
pub mod utils {
    use super::*;
    
    /// Create a shutdown coordinator with default timeout
    pub fn create_shutdown_coordinator() -> ShutdownCoordinator {
        ShutdownCoordinator::new(Duration::from_secs(30))
    }
    
    /// Setup signal handlers for graceful shutdown
    pub async fn setup_shutdown_handlers(coordinator: Arc<ShutdownCoordinator>) {
        coordinator.monitor_signals().await;
    }
    
    /// Graceful task spawner that respects shutdown
    pub fn spawn_managed_task<F>(
        coordinator: &ShutdownCoordinator,
        name: &str,
        task: F,
    ) -> Option<tokio::task::JoinHandle<()>>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        if let Some(_tracker) = coordinator.track_task() {
            let task_name = name.to_string();
            Some(tokio::spawn(async move {
                tracing::debug!("Starting managed task: {}", task_name);
                task.await;
                tracing::debug!("Completed managed task: {}", task_name);
                // _tracker is dropped here, automatically decrementing task count
            }))
        } else {
            warn!("Cannot spawn task '{}' during shutdown", name);
            None
        }
    }
}

/// Connection-aware shutdown for network services
pub struct ConnectionAwareShutdown {
    coordinator: Arc<ShutdownCoordinator>,
    active_connections: Arc<Semaphore>,
}

impl ConnectionAwareShutdown {
    pub fn new(coordinator: Arc<ShutdownCoordinator>) -> Self {
        Self {
            coordinator,
            active_connections: Arc::new(Semaphore::new(0)),
        }
    }
    
    /// Track a new connection
    pub fn track_connection(&self) -> Option<ConnectionTracker> {
        if self.coordinator.is_shutting_down() {
            return None;
        }
        
        if let Ok(permit) = self.active_connections.try_acquire() {
            Some(ConnectionTracker {
                _permit: permit,
                coordinator: Arc::clone(&self.coordinator),
            })
        } else {
            None
        }
    }
    
    /// Wait for all connections to close
    pub async fn wait_for_connections(&self, timeout: Duration) {
        let start = std::time::Instant::now();
        
        while start.elapsed() < timeout {
            if self.active_connections.available_permits() == 0 {
                info!("✅ All connections closed");
                return;
            }
            
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        
        warn!("⚠️  Some connections did not close within timeout");
    }
}

/// RAII connection tracker
pub struct ConnectionTracker {
    _permit: tokio::sync::SemaphorePermit<'static>,
    coordinator: Arc<ShutdownCoordinator>,
}

impl ConnectionTracker {
    /// Check if shutdown is requested
    pub fn should_shutdown(&self) -> bool {
        self.coordinator.is_shutting_down()
    }
    
    /// Get shutdown signal receiver
    pub fn subscribe_shutdown(&self) -> broadcast::Receiver<()> {
        self.coordinator.subscribe()
    }
}

/// Prebuilt shutdown handlers for common components
pub mod handlers {
    use super::*;
    
    /// TCP server shutdown handler
    pub fn tcp_server_handler(
        server_handle: tokio::task::JoinHandle<()>,
    ) -> impl Fn() -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        move || {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let server_handle = &server_handle;
            
            tokio::spawn(async move {
                server_handle.abort();
                let result = match server_handle.await {
                    Ok(()) => Ok(()),
                    Err(e) if e.is_cancelled() => Ok(()), // Expected for abort
                    Err(e) => Err(format!("Server shutdown error: {}", e)),
                };
                let _ = tx.send(result);
            });
            
            rx
        }
    }
    
    /// HTTP server shutdown handler  
    pub fn http_server_handler(
        shutdown_tx: tokio::sync::oneshot::Sender<()>,
    ) -> impl Fn() -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        move || {
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            
            if let Err(_) = shutdown_tx.send(()) {
                let _ = result_tx.send(Err("Failed to send shutdown signal".to_string()));
            } else {
                let _ = result_tx.send(Ok(()));
            }
            
            result_rx
        }
    }
    
    /// Database connection shutdown handler
    pub fn database_handler<F>(
        cleanup_fn: F,
    ) -> impl Fn() -> tokio::sync::oneshot::Receiver<Result<(), String>>
    where
        F: Fn() -> Result<(), String> + Send + 'static,
    {
        move || {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let cleanup_fn = cleanup_fn;
            
            tokio::spawn(async move {
                let result = cleanup_fn();
                let _ = tx.send(result);
            });
            
            rx
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_shutdown_coordinator() {
        let coordinator = ShutdownCoordinator::new(Duration::from_secs(5));
        
        // Test task tracking
        let tracker = coordinator.track_task();
        assert!(tracker.is_some());
        
        // Test shutdown initiation
        assert!(!coordinator.is_shutting_down());
        
        let shutdown_result = coordinator.shutdown().await;
        assert!(shutdown_result.is_ok());
        assert!(coordinator.is_shutting_down());
    }
    
    #[tokio::test]
    async fn test_connection_tracking() {
        let coordinator = Arc::new(ShutdownCoordinator::new(Duration::from_secs(5)));
        let connection_shutdown = ConnectionAwareShutdown::new(coordinator);
        
        let tracker = connection_shutdown.track_connection();
        assert!(tracker.is_some());
        
        drop(tracker);
        
        // Should complete quickly since no active connections
        connection_shutdown.wait_for_connections(Duration::from_secs(1)).await;
    }
}