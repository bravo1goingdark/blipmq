//! CLI client for BlipMQ broker.
//!
//! Provides a convenient command-line interface for publishing, subscribing, and
//! unsubscribing from topics on a running BlipMQ instance.

use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::info;

use blipmq::core::command::{encode_command, new_pub, new_sub, new_unsub};
use blipmq::core::message::decode_message;

/// Command-line interface for BlipMQ.
#[derive(Debug, Parser)]
#[command(
    name = "blipmq-cli",
    version,
    about = "BlipMQ CLI: pub/sub/unsub commands"
)]
pub struct Cli {
    /// Address of the BlipMQ broker (e.g. 127.0.0.1:6379)
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
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let mut stream = TcpStream::connect(cli.addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to {}: {}", cli.addr, e))?;

    let (read_half, mut write_half) = stream.split();
    let reader = BufReader::new(read_half).lines();

    // Send command in protobuf format
    let cmd = match &cli.command {
        Command::Pub { topic, message } => new_pub(topic.clone(), message.clone()),
        Command::Sub { topic } => new_sub(topic.clone()),
        Command::Unsub { topic } => new_unsub(topic.clone()),
    };

    let encoded = encode_command(&cmd);
    let len_prefix = (encoded.len() as u32).to_be_bytes();
    write_half.write_all(&len_prefix).await?;
    write_half.write_all(&encoded).await?;
    info!("Sent request: {:?}", cmd.action);

    let mut lines = reader;
    // Handle subscription with binary Protobuf decoding
    if matches!(cli.command, Command::Pub { .. } | Command::Sub { .. }) {
        // ✅ Step 1: Read and print OK SUB line
        if let Ok(Some(line)) = lines.next_line().await {
            println!("> {}", line.trim());
        } else {
            println!("> Failed to receive subscription confirmation");
            return Ok(());
        }

        // ✅ Step 2: Switch to raw binary mode
        let mut raw_reader = lines.into_inner();

        loop {
            let mut len_buf = [0u8; 4];
            if raw_reader.read_exact(&mut len_buf).await.is_err() {
                break;
            }
            let msg_len = u32::from_be_bytes(len_buf) as usize;

            let mut msg_buf = vec![0u8; msg_len];
            if raw_reader.read_exact(&mut msg_buf).await.is_err() {
                break;
            }

            if let Ok(message) = decode_message(&msg_buf) {
                let payload = String::from_utf8_lossy(&message.payload);
                println!("{} {} {}", message.id, payload, message.timestamp);
            }
        }

        info!("Subscription ended");
    }

    Ok(())
}
