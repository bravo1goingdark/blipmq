//! Production-optimized server with all performance enhancements.

use crate::config::CONFIG;
use crate::core::{
    advanced_config::ProductionConfig,
    command::{decode_command, Action, ClientCommand},
    connection_manager::{ConnectionManager, ConnectionLimits},
    memory_pool::{acquire_medium_buffer, PERFORMANCE_METRICS as POOL_METRICS},
    message::{encode_frame_into, new_message_with_ttl, ServerFrame, SubAck},
    performance::{LatencyTimer, PERFORMANCE_METRICS},
    subscriber::{Subscriber, SubscriberId},
    topics::TopicRegistry,
    vectored_io::{BatchWriter, OptimizedConnection},
};

use bytes::BytesMut;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::{Mutex, RwLock};
use tokio::{select, task, time};
use tracing::{error, info, warn};

/// Production-grade server with advanced optimizations
#[derive(Debug)]
pub struct OptimizedServer {
    config: ProductionConfig,
    registry: Arc<TopicRegistry>,
    connection_manager: Arc<ConnectionManager>,
    shutdown_signal: Arc<tokio::sync::broadcast::Sender<()>>,
}

impl OptimizedServer {
    pub fn new(config: ProductionConfig) -> Self {
        let connection_limits = ConnectionLimits {
            max_connections: config.server.max_connections,
            max_connections_per_ip: config.server.max_connections_per_ip,
            connection_timeout: Duration::from_millis(config.server.connection_timeout_ms),
            idle_timeout: Duration::from_millis(config.server.idle_timeout_ms),
            rate_limit_per_connection: config.security.rate_limit_per_connection,
            rate_limit_window: Duration::from_millis(config.security.rate_limit_window_ms),
        };
        
        let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);
        
        Self {
            config,
            registry: Arc::new(TopicRegistry::new()),
            connection_manager: Arc::new(ConnectionManager::new(connection_limits)),
            shutdown_signal: Arc::new(shutdown_tx),
        }
    }
    
    /// Start the optimized server
    pub async fn serve(&self) -> anyhow::Result<()> {
        let bind_addr = &self.config.server.bind_addr;
        info!("🚀 Starting optimized BlipMQ server on {}", bind_addr);
        
        // Create optimized TCP listener
        let listener = self.create_optimized_listener(bind_addr).await?;
        
        // Start background tasks
        self.start_background_tasks().await;
        
        // Start performance monitoring
        if self.config.monitoring.enable_metrics {
            let perf_task = crate::core::performance::start_performance_monitor();
            task::spawn(perf_task);
        }
        
        // Main accept loop with graceful shutdown
        let mut shutdown_rx = self.shutdown_signal.subscribe();
        
        loop {
            select! {
                result = listener.accept() => {
                    match result {
                        Ok((socket, peer_addr)) => {
                            if let Err(e) = self.handle_new_connection(socket, peer_addr).await {
                                warn!("Failed to handle connection from {}: {}", peer_addr, e);
                            }
                        }
                        Err(e) => {
                            error!("Accept error: {}", e);
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    info!("Received shutdown signal, stopping server gracefully");
                    break;
                }
            }
        }
        
        self.shutdown_gracefully().await;
        Ok(())
    }
    
    /// Create an optimized TCP listener with production settings
    async fn create_optimized_listener(&self, bind_addr: &str) -> anyhow::Result<TcpListener> {
        let addr: SocketAddr = bind_addr.parse()?;
        
        // Create socket with advanced options
        let socket = if addr.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };
        
        // Set socket options for performance
        socket.set_reuseaddr(true)?;
        if self.config.server.reuse_port {
            #[cfg(unix)]
            {
                use std::os::unix::io::AsRawFd;
                unsafe {
                    let optval: libc::c_int = 1;
                    libc::setsockopt(
                        socket.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_REUSEPORT,
                        &optval as *const _ as *const libc::c_void,
                        std::mem::size_of_val(&optval) as libc::socklen_t,
                    );
                }
            }
        }
        
        // Bind and listen with optimized backlog
        socket.bind(addr)?;
        let listener = socket.listen(self.config.server.backlog)?;
        
        info!("📡 TCP listener created with backlog {}", self.config.server.backlog);
        Ok(listener)
    }
    
    /// Handle new connection with rate limiting and optimization
    async fn handle_new_connection(&self, socket: TcpStream, peer_addr: SocketAddr) -> anyhow::Result<()> {
        let timer = LatencyTimer::start();
        
        // Try to accept connection (rate limiting happens here)
        let connection_state = self.connection_manager
            .try_accept_connection(peer_addr)
            .await
            .map_err(|e| anyhow::anyhow!("Connection rejected: {}", e))?;
        
        // Optimize TCP socket
        self.optimize_tcp_socket(&socket)?;
        
        // Spawn connection handler
        let registry = Arc::clone(&self.registry);
        let connection_manager = Arc::clone(&self.connection_manager);
        let config = self.config.clone();
        
        task::spawn(async move {
            let result = Self::handle_optimized_client(
                socket,
                peer_addr,
                registry,
                connection_manager,
                config,
                connection_state,
            ).await;
            
            if let Err(e) = result {
                error!("Client {} error: {:?}", peer_addr, e);
            }
        });
        
        timer.record_publish_latency(); // Record connection accept latency
        Ok(())
    }
    
    /// Optimize TCP socket settings for performance
    fn optimize_tcp_socket(&self, socket: &TcpStream) -> anyhow::Result<()> {
        if self.config.server.tcp_nodelay {
            socket.set_nodelay(true)?;
        }
        
        // Set buffer sizes if configured
        if let Some(recv_buf) = self.config.server.socket_recv_buffer_size {
            #[cfg(unix)]
            {
                use std::os::unix::io::AsRawFd;
                unsafe {
                    let optval = recv_buf as libc::c_int;
                    libc::setsockopt(
                        socket.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_RCVBUF,
                        &optval as *const _ as *const libc::c_void,
                        std::mem::size_of_val(&optval) as libc::socklen_t,
                    );
                }
            }
        }
        
        if let Some(send_buf) = self.config.server.socket_send_buffer_size {
            #[cfg(unix)]
            {
                use std::os::unix::io::AsRawFd;
                unsafe {
                    let optval = send_buf as libc::c_int;
                    libc::setsockopt(
                        socket.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_SNDBUF,
                        &optval as *const _ as *const libc::c_void,
                        std::mem::size_of_val(&optval) as libc::socklen_t,
                    );
                }
            }
        }
        
        Ok(())
    }
    
    /// Optimized client handler with memory pooling and vectored I/O
    async fn handle_optimized_client(
        stream: TcpStream,
        peer_addr: SocketAddr,
        registry: Arc<TopicRegistry>,
        connection_manager: Arc<ConnectionManager>,
        config: ProductionConfig,
        _connection_state: Arc<crate::core::connection_manager::ConnectionState>,
    ) -> anyhow::Result<()> {
        info!("🔗 Optimized client connected: {}", peer_addr);
        
        // Split stream for optimized I/O
        let (reader, writer) = stream.into_split();
        let shared_writer = Arc::new(Mutex::new(BufWriter::with_capacity(
            config.networking.write_buffer_size,
            writer
        )));
        
        // Create optimized connection
        let buf_reader = BufReader::with_capacity(config.networking.read_buffer_size, reader);
        let mut optimized_conn = OptimizedConnection::new(buf_reader, ());
        
        let subscriber_id = SubscriberId::from(peer_addr.to_string());
        let subscriber = Subscriber::new(subscriber_id.clone(), shared_writer.clone());
        
        // Use pooled buffers for processing
        let mut frame_buffer = acquire_medium_buffer();
        let mut subscriptions = Vec::new();
        
        loop {
            // Read messages using optimized connection
            let messages = optimized_conn.read_messages().await?;
            if messages.is_empty() {
                break; // EOF
            }
            
            // Process messages in batch
            for message_bytes in messages {
                let timer = LatencyTimer::start();
                
                // Check rate limit
                if !connection_manager.check_rate_limit(peer_addr) {
                    warn!("Rate limit exceeded for {}", peer_addr);
                    continue;
                }
                
                // Update connection activity
                connection_manager.update_activity(peer_addr);
                
                // Decode command
                let cmd: ClientCommand = match decode_command(&message_bytes) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                
                // Process command
                match cmd.action {
                    Action::Pub => {
                        if let Some(topic) = registry.get_topic(&cmd.topic) {
                            let message = Arc::new(new_message_with_ttl(cmd.payload, cmd.ttl_ms));
                            topic.publish(message).await;
                            
                            // Update metrics
                            PERFORMANCE_METRICS.messages_published.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    
                    Action::Sub => {
                        // Create SubAck response
                        let ack = SubAck {
                            topic: cmd.topic.clone(),
                            info: "subscribed".into(),
                        };
                        let frame = ServerFrame::SubAck(ack);
                        
                        // Use pooled buffer for encoding
                        frame_buffer.as_mut().clear();
                        encode_frame_into(&frame, frame_buffer.as_mut());
                        
                        // Send response
                        {
                            let mut w = shared_writer.lock().await;
                            w.write_all(frame_buffer.as_ref().as_bytes()).await?;
                            w.flush().await?;
                        }
                        
                        // Register subscription
                        let topic = registry.create_or_get_topic(&cmd.topic);
                        topic.subscribe(subscriber.clone(), config.queues.subscriber_capacity).await;
                        subscriptions.push(cmd.topic.clone());
                    }
                    
                    Action::Unsub => {
                        if let Some(topic) = registry.get_topic(&cmd.topic) {
                            topic.unsubscribe(&subscriber_id).await;
                        }
                    }
                    
                    Action::Quit => break,
                }
                
                timer.record_delivery_latency();
            }
        }
        
        // Cleanup subscriptions
        for topic_name in subscriptions {
            if let Some(topic) = registry.get_topic(&topic_name) {
                topic.unsubscribe(&subscriber_id).await;
            }
        }
        
        // Remove connection
        connection_manager.remove_connection(peer_addr);
        info!("🔌 Client disconnected: {}", peer_addr);
        
        Ok(())
    }
    
    /// Start background maintenance tasks
    async fn start_background_tasks(&self) {
        // Connection cleanup task
        let connection_manager = Arc::clone(&self.connection_manager);
        task::spawn(async move {
            connection_manager.cleanup_idle_connections().await;
        });
        
        // Memory pool statistics reporting
        if self.config.monitoring.enable_metrics {
            task::spawn(async move {
                let mut interval = time::interval(Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    let buffer_hit_rate = POOL_METRICS.buffer_hit_rate();
                    let message_hit_rate = POOL_METRICS.message_hit_rate();
                    info!("Memory pools - Buffer hit rate: {:.2}%, Message hit rate: {:.2}%", 
                          buffer_hit_rate * 100.0, message_hit_rate * 100.0);
                }
            });
        }
        
        // System resource monitoring
        if self.config.monitoring.enable_health_checks {
            task::spawn(async move {
                let mut interval = time::interval(
                    Duration::from_millis(self.config.monitoring.health_check_interval_ms)
                );
                loop {
                    interval.tick().await;
                    Self::check_system_health().await;
                }
            });
        }
    }
    
    /// System health monitoring
    async fn check_system_health() {
        // Check memory usage
        let allocated_bytes = crate::core::memory_pool::POOL_STATS
            .buffer_pool_hits.load(std::sync::atomic::Ordering::Relaxed);
        
        // Check active connections
        let active_connections = PERFORMANCE_METRICS
            .active_connections.load(std::sync::atomic::Ordering::Relaxed);
        
        // Log health status
        if active_connections > 0 {
            info!("Health: {} active connections, {} bytes allocated", 
                  active_connections, allocated_bytes);
        }
    }
    
    /// Graceful shutdown process
    async fn shutdown_gracefully(&self) {
        info!("🛑 Starting graceful shutdown...");
        
        // Wait for connections to finish (with timeout)
        let shutdown_timeout = Duration::from_secs(30);
        let start_time = std::time::Instant::now();
        
        while start_time.elapsed() < shutdown_timeout {
            let active = PERFORMANCE_METRICS
                .active_connections.load(std::sync::atomic::Ordering::Relaxed);
            
            if active == 0 {
                break;
            }
            
            info!("Waiting for {} connections to finish...", active);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        
        info!("✅ Graceful shutdown completed");
    }
    
    /// Signal shutdown
    pub async fn shutdown(&self) {
        let _ = self.shutdown_signal.send(());
    }
}

/// Configuration validation and auto-tuning
pub fn create_optimized_server() -> anyhow::Result<OptimizedServer> {
    // Load production configuration
    let mut config = ProductionConfig::load_from_env_or_file(Some("production.toml"))
        .unwrap_or_else(|_| {
            warn!("Could not load production.toml, using balanced profile");
            ProductionConfig::balanced_profile()
        });
    
    // Auto-tune based on system capabilities
    config.auto_tune();
    
    // Validate configuration
    let warnings = config.validate();
    for warning in warnings {
        warn!("Configuration warning: {}", warning);
    }
    
    // Log configuration profile
    info!("Using profile: {:?}", config.profile);
    info!("Worker threads: {}", config.performance.worker_threads);
    info!("Max connections: {}", config.server.max_connections);
    info!("Memory pooling: {}", config.memory.enable_memory_pooling);
    info!("Vectored I/O: {}", config.networking.enable_vectored_io);
    
    Ok(OptimizedServer::new(config))
}

/// Public function to start the optimized server
pub async fn serve_optimized() -> anyhow::Result<()> {
    let server = create_optimized_server()?;
    server.serve().await
}