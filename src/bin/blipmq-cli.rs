//! CLI client for BlipMQ broker.
//!
//! Provides a convenient command-line interface for publishing, subscribing, and unsubscribing
//! to topics on a running BlipMQ instance.
//!
//! All commands are routed through a single entrypoint (`blipmq-cli`).
//!
//! # Usage Examples
//!
//! Publish a message:
//! ```bash
//! blipmq-cli --addr 127.0.0.1:6379 pub mytopic "hello world"
//! ```
//!
//! Subscribe to a topic:
//! ```bash
//! blipmq-cli --addr 127.0.0.1:6379 sub mytopic
//! ```
//!
//! Unsubscribe from a topic:
//! ```bash
//! blipmq-cli --addr 127.0.0.1:6379 unsub mytopic
//! ```

use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::info;

/// Command-line interface for BlipMQ.
#[derive(Debug, Parser)]
#[command(
    name = "blipmq-cli",
    version,
    about = "BlipMQ CLI: pub/sub/unsub commands"
)]
pub struct Cli {
    /// Address of the BlipMQ broker (e.g. 127.0.0.1:6379)
    #[arg(short, long, default_value = "127.0.0.1:8080")]    pub addr: SocketAddr,

    #[command(subcommand)]
    pub command: Command,
}

/// Supported CLI subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Publish a message to a topic
    Pub {
        /// Topic name
        topic: String,
        /// Message payload (enclose in quotes for spaces)
        message: String,
    },

    /// Subscribe to messages on a topic
    Sub {
        /// Topic name
        topic: String,
    },

    /// Unsubscribe from a topic
    Unsub {
        /// Topic name
        topic: String,
    },
}

/// Entrypoint: parse CLI args and dispatch commands.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing subscriber for logging
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let mut stream = TcpStream::connect(cli.addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to {}: {}", cli.addr, e))?;

    let (read_half, mut write_half) = stream.split();
    let mut reader = BufReader::new(read_half).lines();

    // Format and send the request line
    let request = match &cli.command {
        Command::Pub { topic, message } => format!("PUB {} {}\n", topic, message),
        Command::Sub { topic } => format!("SUB {}\n", topic),
        Command::Unsub { topic } => format!("UNSUB {}\n", topic),
    };

    write_half.write_all(request.as_bytes()).await?;
    info!("Sent request: {}", request.trim());

    // If subscribing, continuously read messages from broker
    if matches!(cli.command, Command::Sub { .. }) {
        while let Ok(Some(line)) = reader.next_line().await {
            println!("{}", line);
        }
        info!("Subscription ended");
    }

    Ok(())
}
