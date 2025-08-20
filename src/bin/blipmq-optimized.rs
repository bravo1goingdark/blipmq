//! Production-optimized BlipMQ binary with advanced performance features.
//!
//! This binary provides production-grade message queuing with:
//! - Memory pooling and zero-copy optimizations
//! - Vectored I/O for high throughput
//! - Connection management with rate limiting
//! - Advanced monitoring and health checks
//! - Graceful shutdown handling
//! - Multiple performance profiles
//!
//! Usage:
//!   blipmq-optimized --profile low-latency
//!   blipmq-optimized --config production.toml
//!   blipmq-optimized --bind 0.0.0.0:8080 --profile high-throughput

use blipmq::{
    BrokerBuilder,
    core::advanced_config::{ProductionConfig, PerformanceProfile},
    core::graceful_shutdown::{ShutdownCoordinator, utils::setup_shutdown_handlers},
    start_monitoring,
};

use clap::{Parser, ValueEnum};
use std::sync::Arc;
use tracing::{info, error, warn};

#[derive(Debug, Parser)]
#[command(
    name = "blipmq-optimized",
    version,
    about = "Production-optimized BlipMQ message broker",
    long_about = "High-performance message broker with advanced optimizations for production deployments"
)]
struct Cli {
    /// Performance profile to use
    #[arg(short, long, value_enum, default_value_t = ProfileArg::Balanced)]
    profile: ProfileArg,
    
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<String>,
    
    /// Bind address (overrides config file)
    #[arg(short, long)]
    bind: Option<String>,
    
    /// Maximum connections (overrides config file)
    #[arg(long)]
    max_connections: Option<usize>,
    
    /// Enable auto-tuning based on system capabilities
    #[arg(long, default_value_t = true)]
    auto_tune: bool,
    
    /// Enable memory pooling
    #[arg(long, default_value_t = true)]
    memory_pooling: bool,
    
    /// Enable vectored I/O
    #[arg(long, default_value_t = true)]
    vectored_io: bool,
    
    /// Enable monitoring and metrics
    #[arg(long, default_value_t = true)]
    monitoring: bool,
    
    /// Metrics server bind address
    #[arg(long, default_value = "127.0.0.1:9090")]
    metrics_addr: String,
    
    /// Validate configuration and exit
    #[arg(long)]
    validate_config: bool,
    
    /// Print configuration template for the profile and exit
    #[arg(long)]
    print_config: bool,
    
    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
    
    /// Disable color output
    #[arg(long)]
    no_color: bool,
}

#[derive(Debug, Clone, ValueEnum)]
enum ProfileArg {
    LowLatency,
    HighThroughput,
    Balanced,
    ResourceEfficient,
}

impl From<ProfileArg> for PerformanceProfile {
    fn from(profile: ProfileArg) -> Self {
        match profile {
            ProfileArg::LowLatency => PerformanceProfile::LowLatency,
            ProfileArg::HighThroughput => PerformanceProfile::HighThroughput,
            ProfileArg::Balanced => PerformanceProfile::Balanced,
            ProfileArg::ResourceEfficient => PerformanceProfile::ResourceEfficient,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    
    // Initialize logging
    initialize_logging(&cli);
    
    // Print welcome banner
    print_banner();
    
    // Load or create configuration
    let mut config = load_configuration(&cli)?;
    
    // Handle special modes
    if cli.print_config {
        println!("{}", config.to_toml());
        return Ok(());
    }
    
    if cli.validate_config {
        validate_and_exit(&config)?;
    }
    
    // Apply CLI overrides
    apply_cli_overrides(&mut config, &cli);
    
    // Set up graceful shutdown
    let shutdown_coordinator = Arc::new(ShutdownCoordinator::new(
        std::time::Duration::from_secs(30)
    ));
    setup_shutdown_handlers(Arc::clone(&shutdown_coordinator)).await;
    
    // Create broker builder with configuration
    let mut builder = BrokerBuilder::new().with_profile(cli.profile.into());
    
    // Apply configuration overrides
    if let Some(bind_addr) = cli.bind {
        builder = builder.bind_addr(bind_addr);
    }
    
    if let Some(max_conn) = cli.max_connections {
        builder = builder.max_connections(max_conn);
    }
    
    builder = builder
        .memory_pooling(cli.memory_pooling)
        .vectored_io(cli.vectored_io)
        .monitoring(cli.monitoring);
    
    if cli.auto_tune {
        builder = builder.auto_tune();
    }
    
    // Load configuration file if provided
    if let Some(config_path) = cli.config {
        match builder.load_config(&config_path) {
            Ok(b) => builder = b,
            Err(e) => {
                warn!("Could not load config file '{}': {}", config_path, e);
                warn!("Using default configuration with selected profile");
            }
        }
    }
    
    let final_config = builder.config();
    
    // Print configuration summary
    print_config_summary(final_config);
    
    // Start monitoring if enabled
    if cli.monitoring {
        let metrics_addr = cli.metrics_addr.clone();
        let shutdown_rx = shutdown_coordinator.subscribe();
        
        tokio::spawn(async move {
            tokio::select! {
                result = start_monitoring(metrics_addr.clone()) => {
                    if let Err(e) = result {
                        error!("Monitoring server error: {}", e);
                    }
                }
                _ = shutdown_rx => {
                    info!("Monitoring server shutting down");
                }
            }
        });
    }
    
    // Start the broker
    info!("🚀 Starting production BlipMQ broker...");
    
    let server_task = tokio::spawn(async move {
        if let Err(e) = builder.start().await {
            error!("Broker error: {}", e);
        }
    });
    
    // Wait for shutdown signal
    let mut shutdown_rx = shutdown_coordinator.subscribe();
    tokio::select! {
        _ = shutdown_rx.recv() => {
            info!("Shutdown signal received");
        }
        result = server_task => {
            match result {
                Ok(()) => info!("Broker completed normally"),
                Err(e) => error!("Broker task error: {}", e),
            }
        }
    }
    
    // Perform graceful shutdown
    info!("🛑 Initiating graceful shutdown...");
    if let Err(e) = shutdown_coordinator.shutdown().await {
        error!("Shutdown error: {}", e);
    }
    
    info!("✅ BlipMQ shut down successfully");
    Ok(())
}

fn initialize_logging(cli: &Cli) {
    let level = if cli.verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };
    
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false);
    
    if cli.no_color {
        subscriber.without_time().init();
    } else {
        subscriber.init();
    }
}

fn print_banner() {
    println!(r#"
 ____  _ _       __  __  ___
| __ )| (_)_ __ |  \/  |/ _ \
|  _ \| | | '_ \| |\/| | | | |
| |_) | | | |_) | |  | | |_| |
|____/|_|_| .__/|_|  |_|\___/
          |_|

Production-Optimized Message Broker
Version: {} | Rust: {} | Build: {}
"#, 
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_RUST_VERSION", "unknown"),
        option_env!("BUILD_TIMESTAMP").unwrap_or("dev")
    );
}

fn load_configuration(cli: &Cli) -> Result<ProductionConfig, Box<dyn std::error::Error>> {
    if let Some(config_path) = &cli.config {
        info!("Loading configuration from: {}", config_path);
        ProductionConfig::load_from_env_or_file(Some(config_path))
            .map_err(Into::into)
    } else {
        // Create config based on profile
        let profile: PerformanceProfile = cli.profile.clone().into();
        let config = match profile {
            PerformanceProfile::LowLatency => ProductionConfig::low_latency_profile(),
            PerformanceProfile::HighThroughput => ProductionConfig::high_throughput_profile(),
            PerformanceProfile::Balanced => ProductionConfig::balanced_profile(),
            PerformanceProfile::ResourceEfficient => ProductionConfig::resource_efficient_profile(),
            PerformanceProfile::Custom => ProductionConfig::balanced_profile(),
        };
        Ok(config)
    }
}

fn validate_and_exit(config: &ProductionConfig) -> Result<(), Box<dyn std::error::Error>> {
    info!("Validating configuration...");
    
    let warnings = config.validate();
    
    if warnings.is_empty() {
        println!("✅ Configuration is valid");
    } else {
        println!("⚠️  Configuration warnings:");
        for warning in warnings {
            println!("   - {}", warning);
        }
    }
    
    // Print configuration summary
    println!("\nConfiguration Summary:");
    println!("  Profile: {:?}", config.profile);
    println!("  Bind Address: {}", config.server.bind_addr);
    println!("  Max Connections: {}", config.server.max_connections);
    println!("  Memory Pooling: {}", config.memory.enable_memory_pooling);
    println!("  Vectored I/O: {}", config.networking.enable_vectored_io);
    println!("  Worker Threads: {}", config.performance.worker_threads);
    
    std::process::exit(0);
}

fn apply_cli_overrides(config: &mut ProductionConfig, cli: &Cli) {
    if let Some(bind_addr) = &cli.bind {
        config.server.bind_addr = bind_addr.clone();
    }
    
    if let Some(max_connections) = cli.max_connections {
        config.server.max_connections = max_connections;
    }
    
    config.memory.enable_memory_pooling = cli.memory_pooling;
    config.networking.enable_vectored_io = cli.vectored_io;
    config.monitoring.enable_metrics = cli.monitoring;
    config.monitoring.metrics_bind_addr = cli.metrics_addr.clone();
}

fn print_config_summary(config: &ProductionConfig) {
    info!("Configuration Summary:");
    info!("  📋 Profile: {:?}", config.profile);
    info!("  🌐 Bind Address: {}", config.server.bind_addr);
    info!("  🔌 Max Connections: {}", config.server.max_connections);
    info!("  🧠 Memory Pooling: {}", config.memory.enable_memory_pooling);
    info!("  📤 Vectored I/O: {}", config.networking.enable_vectored_io);
    info!("  👷 Worker Threads: {}", config.performance.worker_threads);
    info!("  📊 Monitoring: {} ({})", config.monitoring.enable_metrics, config.monitoring.metrics_bind_addr);
    info!("  🔒 Authentication: {}", config.security.enable_authentication);
    info!("  🚦 Rate Limiting: {} ({}/conn, {}/IP)", 
          config.security.enable_rate_limiting,
          config.security.rate_limit_per_connection,
          config.security.rate_limit_per_ip);
}

/// Print usage examples
#[allow(dead_code)]
fn print_usage_examples() {
    println!(r#"
Usage Examples:

  # Start with low latency profile
  blipmq-optimized --profile low-latency --bind 0.0.0.0:8080

  # Start with high throughput profile  
  blipmq-optimized --profile high-throughput --max-connections 50000

  # Use custom configuration file
  blipmq-optimized --config production.toml

  # Resource efficient mode for containers
  blipmq-optimized --profile resource-efficient

  # Validate configuration without starting
  blipmq-optimized --config production.toml --validate-config

  # Generate configuration template
  blipmq-optimized --profile low-latency --print-config > low-latency.toml

  # Development mode with verbose logging
  blipmq-optimized --verbose --monitoring --metrics-addr 127.0.0.1:9090
"#);
}