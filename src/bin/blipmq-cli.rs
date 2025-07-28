//! CLI client for BlipMQ broker.
//!
//! Provides a convenient command-line interface for publishing, subscribing, and
//! unsubscribing from topics on a running BlipMQ instance, with support for
//! configurable per-message TTLs via blipmq.toml.

use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::info;

use blipmq::config::CONFIG;
use blipmq::core::command::{encode_command, new_pub_with_ttl, new_sub, new_unsub};
use blipmq::core::message::decode_message;

/// Command-line interface for BlipMQ.
#[derive(Debug, Parser)]
#[command(
    name = "blipmq-cli",
    version,
    about = "BlipMQ CLI: pub/sub/unsub commands with TTL support"
)]
pub struct Cli {
    /// Address of the BlipMQ broker (e.g. 127.0.0.1:8080)
    #[arg(short, long, default_value = "127.0.0.1:8080")]
    pub addr: SocketAddr,

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
        /// Optional TTL in milliseconds (defaults to CONFIG.delivery.default_ttl_ms)
        #[arg(short, long)]
        ttl: Option<u64>,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let mut stream = TcpStream::connect(cli.addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to {}: {}", cli.addr, e))?;

    let (read_half, mut write_half) = stream.split();
    let mut lines = BufReader::new(read_half).lines();

    // Build the appropriate command, using TTL override or default
    let cmd = match &cli.command {
        Command::Pub {
            topic,
            ttl,
            message,
        } => {
            let ttl_ms = ttl.unwrap_or(CONFIG.delivery.default_ttl_ms);
            new_pub_with_ttl(topic.clone(), message.clone(), ttl_ms)
        }
        Command::Sub { topic } => new_sub(topic.clone()),
        Command::Unsub { topic } => new_unsub(topic.clone()),
    };

    // Encode and send
    let encoded = encode_command(&cmd);
    write_half
        .write_all(&(encoded.len() as u32).to_be_bytes())
        .await?;
    write_half.write_all(&encoded).await?;
    info!("Sent request: action={}", cmd.action);

    // For Pub and Sub, read confirmation and then raw messages
    if matches!(cli.command, Command::Pub { .. } | Command::Sub { .. }) {
        if let Ok(Some(line)) = lines.next_line().await {
            println!("> {}", line.trim());
        } else {
            eprintln!("> Failed to receive broker confirmation");
            return Ok(());
        }

        // Now switch to raw binary reading
        let mut raw = lines.into_inner();
        loop {
            let mut len_buf = [0u8; 4];
            if raw.read_exact(&mut len_buf).await.is_err() {
                break;
            }
            let msg_len = u32::from_be_bytes(len_buf) as usize;
            let mut msg_buf = vec![0u8; msg_len];
            if raw.read_exact(&mut msg_buf).await.is_err() {
                break;
            }
            match decode_message(&msg_buf) {
                Ok(msg) => {
                    let payload = String::from_utf8_lossy(&msg.payload);
                    println!("{} {} @{}", msg.id, payload, msg.timestamp);
                }
                Err(e) => {
                    eprintln!("❌ Failed to decode message: {}", e);
                }
            }
        }

        info!("Subscription ended");
    }

    Ok(())
}
