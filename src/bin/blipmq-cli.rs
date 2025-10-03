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
    about = "🚀 BlipMQ CLI - Ultra-fast message queue operations",
    long_about = "BlipMQ CLI provides simple commands for pub/sub operations:\n\n\
        • pub <topic> <message>  - Publish a message\n\
        • sub <topic>           - Subscribe to messages\n\
        • unsub <topic>         - Unsubscribe from topic\n\n\
        Examples:\n\
        • blipmq-cli pub chat \"Hello world!\"\n\
        • blipmq-cli sub events\n\
        • blipmq-cli --addr prod-server:7878 pub alerts \"System ready\""
)]
pub struct Cli {
    /// Broker address (host:port)
    #[arg(short, long, default_value = "127.0.0.1:8080", help = "BlipMQ broker address")]
    pub addr: SocketAddr,

    /// Verbose output
    #[arg(short, long, help = "Enable verbose logging")]
    pub verbose: bool,

    /// Quiet mode (errors only)
    #[arg(short, long, help = "Suppress non-error output")]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// Supported CLI subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// 📤 Publish a message to a topic
    #[command(alias = "p")]
    Pub {
        /// Topic name
        #[arg(help = "Target topic for the message")]
        topic: String,
        /// Message payload (use quotes for spaces, '-' for stdin)
        #[arg(help = "Message content to publish")]
        message: String,
        /// TTL in milliseconds (default: server config)
        #[arg(short, long, help = "Message time-to-live in ms")]
        ttl: Option<u64>,
        /// Publish from file instead of command line
        #[arg(short, long, help = "Read message from file")]
        file: Option<PathBuf>,
    },

    /// 📥 Subscribe to messages on a topic  
    #[command(alias = "s")]
    Sub {
        /// Topic name
        #[arg(help = "Topic to subscribe to")]
        topic: String,
        /// Stop after N messages (default: infinite)
        #[arg(short = 'n', long, help = "Exit after receiving N messages")]
        count: Option<usize>,
        /// Output format: text, json
        #[arg(short = 'f', long, default_value = "text", help = "Output format (text|json)")]
        format: String,
    },

    /// ❌ Unsubscribe from a topic
    #[command(alias = "u")]
    Unsub {
        /// Topic name
        #[arg(help = "Topic to unsubscribe from")]
        topic: String,
    },

    /// 📊 Show broker statistics
    #[command(alias = "stats")]
    Info,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    
    // Configure logging based on verbosity
    if cli.verbose {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .init();
    } else if !cli.quiet {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .init();
    }

    let mut stream = TcpStream::connect(cli.addr)
        .await
        .map_err(|e| anyhow::anyhow!("❌ Failed to connect to {}: {}", cli.addr, e))?;
    
    // Low-latency I/O
    let _ = stream.set_nodelay(true);
    
    if !cli.quiet {
        println!("🔗 Connected to BlipMQ at {}", cli.addr);
    }

    let (read_half, mut write_half) = stream.split();
    let mut reader = BufReader::new(read_half);

    // Build and send command
    let cmd = match &cli.command {
        Command::Pub {
            topic,
            message,
            ttl,
            file,
        } => {
            // Determine message source (file, stdin, or direct)
            let resolved = if let Some(file_path) = file {
                // Read from file
                let data = tokio::fs::read(file_path).await
                    .map_err(|e| anyhow::anyhow!("❌ Failed to read file {}: {}", file_path.display(), e))?;
                if data.len() > CONFIG.server.max_message_size_bytes {
                    return Err(anyhow::anyhow!(
                        "❌ File size ({} bytes) exceeds server limit ({} bytes)",
                        data.len(),
                        CONFIG.server.max_message_size_bytes
                    ));
                }
                data
            } else if message == "-" {
                // Read from stdin
                use tokio::io::AsyncReadExt as _;
                if !cli.quiet {
                    eprint!("📝 Reading from stdin (Ctrl+D to finish): ");
                }
                let mut data = Vec::new();
                let mut stdin = tokio::io::stdin();
                stdin.read_to_end(&mut data).await?;
                if data.len() > CONFIG.server.max_message_size_bytes {
                    return Err(anyhow::anyhow!(
                        "❌ Input size ({} bytes) exceeds server limit ({} bytes)",
                        data.len(),
                        CONFIG.server.max_message_size_bytes
                    ));
                }
                data
            } else {
                // Use message from command line
                let data = message.as_bytes().to_vec();
                if data.len() > CONFIG.server.max_message_size_bytes {
                    return Err(anyhow::anyhow!(
                        "❌ Message size ({} bytes) exceeds server limit ({} bytes)",
                        data.len(),
                        CONFIG.server.max_message_size_bytes
                    ));
                }
                data
            };
            
            let ttl_ms = ttl.unwrap_or(CONFIG.delivery.default_ttl_ms);
            if !cli.quiet {
                println!("📤 Publishing {} bytes to '{}' (TTL: {}ms)", resolved.len(), topic, ttl_ms);
            }
            new_pub_with_ttl(topic.clone(), resolved, ttl_ms)
        }
        Command::Sub { topic, .. } => {
            if !cli.quiet {
                println!("📥 Subscribing to '{}'", topic);
            }
            new_sub(topic.clone())
        }
        Command::Unsub { topic } => {
            if !cli.quiet {
                println!("❌ Unsubscribing from '{}'", topic);
            }
            new_unsub(topic.clone())
        }
        Command::Info => {
            // For now, just return early - could be extended to query broker stats
            if !cli.quiet {
                println!("📊 Broker info feature coming soon!");
            }
            return Ok(());
        }
    };

    let frame = encode_command_with_len_prefix(&cmd);
    write_half.write_all(&frame).await?;
    
    if cli.verbose {
        info!("Sent request: action={:?}", cmd.action);
    }

    if let Command::Sub { count, format, .. } = &cli.command {
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
            Ok(ServerFrame::SubAck(ack)) => {
                if !cli.quiet {
                    println!("✅ {}", ack.info);
                }
            }
            Ok(_) => {
                eprintln!("❌ Unexpected frame instead of SubAck");
                return Ok(());
            }
            Err(e) => {
                eprintln!("❌ Failed to decode SubAck: {e}");
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
                    seen += 1;
                    
                    // Format output based on user preference
                    if format == "json" {
                        let json_msg = serde_json::json!({
                            "id": msg.id,
                            "payload": String::from_utf8_lossy(&msg.payload),
                            "timestamp": msg.timestamp,
                            "sequence": seen
                        });
                        println!("{}", json_msg);
                    } else {
                        let payload = String::from_utf8_lossy(&msg.payload);
                        let timestamp = chrono::DateTime::from_timestamp_millis(msg.timestamp as i64)
                            .map(|dt| dt.format("%H:%M:%S%.3f").to_string())
                            .unwrap_or_else(|| msg.timestamp.to_string());
                        println!("💬 [{}] {}: {}", timestamp, msg.id, payload);
                    }
                    
                    if let Some(limit) = count {
                        if seen >= *limit {
                            if !cli.quiet {
                                println!("🏁 Received {} messages, exiting", seen);
                            }
                            break;
                        }
                    }
                }
                Ok(_) => {
                    if cli.verbose {
                        eprintln!("⚠️ Unexpected frame type");
                    }
                }
                Err(e) => {
                    eprintln!("❌ Failed to decode frame: {e}");
                }
            }
        }

        if !cli.quiet {
            println!("ℹ️ Subscription ended (received {} messages)", seen);
        }
    }

    Ok(())
}
