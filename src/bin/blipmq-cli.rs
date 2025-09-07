//! CLI client for BlipMQ broker.
//!
//! Provides a convenient command-line interface for publishing, subscribing, and
//! unsubscribing from topics on a running BlipMQ instance, with support for
//! configurable per-message TTLs via blipmq.toml.

use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{error, info};

use blipmq::config::CONFIG;
use blipmq::core::command::{encode_command_with_len_prefix, new_pub_with_ttl, new_sub, new_unsub};
use blipmq::core::message::{decode_frame, ServerFrame};

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
        /// Message payload (enclose in quotes for spaces). Use '-' to read from STDIN.
        message: String,
    },

    /// Publish the contents of a file as a message
    PubFile {
        /// Topic name
        topic: String,
        /// Optional TTL in milliseconds (defaults to CONFIG.delivery.default_ttl_ms)
        #[arg(short, long)]
        ttl: Option<u64>,
        /// Path to file whose contents will be sent as the message
        file: PathBuf,
    },

    /// Subscribe to messages on a topic
    Sub {
        /// Topic name
        topic: String,
        /// Stop after receiving this many messages (omit to stream indefinitely)
        #[arg(short = 'n', long)]
        count: Option<usize>,
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
    // Low-latency I/O
    let _ = stream.set_nodelay(true);

    let (read_half, mut write_half) = stream.split();
    let mut reader = BufReader::new(read_half);

    // Build and send command
    let cmd = match &cli.command {
        Command::Pub {
            topic,
            ttl,
            message,
        } => {
            // Resolve message (support '-' for stdin)
            let resolved = if message == "-" {
                use tokio::io::AsyncReadExt as _;
                let mut data = Vec::new();
                let mut stdin = tokio::io::stdin();
                stdin.read_to_end(&mut data).await?;
                String::from_utf8_lossy(&data).into_owned()
            } else {
                message.clone()
            };
            // Check message size against server's max_message_size_bytes
            if resolved.len() > CONFIG.server.max_message_size_bytes {
                error!(
                    "Message size ({}) exceeds server's maximum allowed size ({}), aborting.",
                    resolved.len(),
                    CONFIG.server.max_message_size_bytes
                );
                return Err(anyhow::anyhow!("Message too large"));
            }
            let ttl_ms = ttl.unwrap_or(CONFIG.delivery.default_ttl_ms);
            new_pub_with_ttl(topic.clone(), resolved, ttl_ms)
        }
        Command::PubFile { topic, ttl, file } => {
            let data = tokio::fs::read(file).await?;
            if data.len() > CONFIG.server.max_message_size_bytes {
                error!(
                    "File size ({}) exceeds server's maximum allowed size ({}), aborting.",
                    data.len(),
                    CONFIG.server.max_message_size_bytes
                );
                return Err(anyhow::anyhow!("Message too large"));
            }
            let ttl_ms = ttl.unwrap_or(CONFIG.delivery.default_ttl_ms);
            // Send as-is (binary safe)
            let payload: Vec<u8> = data;
            new_pub_with_ttl(topic.clone(), payload, ttl_ms)
        }
        Command::Sub { topic, .. } => new_sub(topic.clone()),
        Command::Unsub { topic } => new_unsub(topic.clone()),
    };

    let frame = encode_command_with_len_prefix(&cmd);
    write_half.write_all(&frame).await?;
    info!("Sent request: action={:?}", cmd.action);

    match &cli.command {
        Command::Sub { count, .. } => {
            // Read SubAck first
            let mut len_buf = [0u8; 4];
            if reader.read_exact(&mut len_buf).await.is_err() {
                eprintln!("> Failed to read SubAck length");
                return Ok(());
            }
            let ack_len = u32::from_be_bytes(len_buf) as usize;

            if ack_len > CONFIG.server.max_message_size_bytes {
                error!(
                    "Server sent oversized SubAck ({} bytes), disconnecting.",
                    ack_len
                );
                return Err(anyhow::anyhow!("Server sent oversized SubAck"));
            }

            let mut ack_buf = vec![0u8; ack_len];
            if reader.read_exact(&mut ack_buf).await.is_err() {
                eprintln!("> Failed to read SubAck payload");
                return Ok(());
            }

            match decode_frame(&ack_buf) {
                Ok(ServerFrame::SubAck(ack)) => println!("> {}", ack.info),
                Ok(_) => {
                    eprintln!("> Unexpected frame instead of SubAck");
                    return Ok(());
                }
                Err(e) => {
                    eprintln!("> Failed to decode SubAck: {e}");
                    return Ok(());
                }
            }

            // Read message frames; optionally stop after N messages
            let mut seen: usize = 0;
            loop {
                if reader.read_exact(&mut len_buf).await.is_err() {
                    break;
                }
                let msg_len = u32::from_be_bytes(len_buf) as usize;

                if msg_len > CONFIG.server.max_message_size_bytes {
                    error!(
                        "Server sent oversized message ({} bytes), disconnecting.",
                        msg_len
                    );
                    break;
                }

                let mut msg_buf = vec![0u8; msg_len];
                if reader.read_exact(&mut msg_buf).await.is_err() {
                    break;
                }

                match decode_frame(&msg_buf) {
                    Ok(ServerFrame::Message(msg)) => {
                        let payload = String::from_utf8_lossy(&msg.payload);
                        println!("{} {} @{}", msg.id, payload, msg.timestamp);
                        seen += 1;
                        if let Some(limit) = count { if seen >= *limit { break; } }
                    }
                    Ok(_) => {
                        eprintln!("⚠️ Unexpected frame");
                    }
                    Err(e) => {
                        eprintln!("❌ Failed to decode frame: {e}");
                    }
                }
            }

            info!("Subscription ended");
        }
        _ => {}
    }

    Ok(())
}
