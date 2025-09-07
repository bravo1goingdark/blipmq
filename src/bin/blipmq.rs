//! blipmq – one binary that can start the broker *or* act as an
//! interactive client shell.
//!
//!  $ blipmq start --config blipmq.toml
//!  $ blipmq connect 127.0.0.1:8080
//!  > pub chat 5000 hello everyone  # custom TTL 5000ms
//!  > pub chat hello everyone       # default TTL from config
//!  > sub chat
//!  > unsub chat
//!  > exit

use blipmq::config::CONFIG;
use blipmq::core::command::{encode_command_with_len_prefix, new_pub_with_ttl, new_sub, new_unsub};
use blipmq::core::message::{decode_frame, ServerFrame};
use blipmq::{load_config, start_broker};

use clap::{Parser, Subcommand};
use rustyline::history::DefaultHistory;
use rustyline::{DefaultEditor, Editor};

use std::net::SocketAddr;
use tokio::task::JoinHandle;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};

#[derive(Debug, Parser)]
#[command(name = "blipmq", version, about = "BlipMQ broker & CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the broker daemon.
    Start {
        /// Path to config TOML (env BLIPMQ_CONFIG overrides)
        #[arg(short, long, default_value = "blipmq.toml")]
        config: String,
    },
    /// Connect to a running broker in interactive mode.
    Connect {
        /// Broker address (host:port)
        addr: SocketAddr,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.cmd {
        Command::Start { config } => {
            let cfg_path = std::env::var("BLIPMQ_CONFIG").unwrap_or(config);
            let cfg = load_config(&cfg_path)?;
            println!("📡 BlipMQ broker listening on {}", cfg.server.bind_addr);
            start_broker().await?;
        }
        Command::Connect { addr } => repl(addr).await?,
    }
    Ok(())
}

async fn repl(addr: SocketAddr) -> anyhow::Result<()> {
    let mut rl: Editor<(), DefaultHistory> = DefaultEditor::new()?;

    let stream = TcpStream::connect(addr).await?;
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);

    let default_ttl = CONFIG.delivery.default_ttl_ms;
    println!("Connected to {addr}. Type `help` for commands.");

    // Background task to print server messages
    let printer: JoinHandle<()> = tokio::spawn(async move {
        loop {
            let mut len_buf = [0u8; 4];
            if reader.read_exact(&mut len_buf).await.is_err() {
                break;
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut msg_buf = vec![0u8; len];
            if reader.read_exact(&mut msg_buf).await.is_err() {
                break;
            }
            match decode_frame(&msg_buf) {
                Ok(ServerFrame::Message(msg)) => {
                    let payload = String::from_utf8_lossy(&msg.payload);
                    println!("{} {} @{}", msg.id, payload, msg.timestamp);
                }
                Ok(ServerFrame::SubAck(ack)) => {
                    println!("> {}", ack.info);
                }
                Err(e) => {
                    eprintln!("❌ Decode error: {e}");
                }
            }
        }
    });

    // Interactive REPL
    while let Ok(line) = rl.readline("> ") {
        rl.add_history_entry(line.as_str())?;

        let parts: Vec<_> = line.split_whitespace().collect();
        match parts.as_slice() {
            ["help"] => {
                println!("Commands:\n  pub <topic> [ttl_ms] <msg...>\n  sub <topic>\n  unsub <topic>\n  exit");
            }

            ["exit" | "quit"] => break,

            // pub with explicit TTL
            ["pub", topic, ttl_str, rest @ ..]
                if ttl_str.parse::<u64>().is_ok() && !rest.is_empty() =>
            {
                let ttl = ttl_str.parse::<u64>()?;
                let payload = rest.join(" ");
                let cmd = new_pub_with_ttl(topic.to_string(), payload, ttl);
                let frame = encode_command_with_len_prefix(&cmd);
                w.write_all(&frame).await?;
            }

            // pub with default TTL
            ["pub", topic, rest @ ..] if !rest.is_empty() => {
                let payload = rest.join(" ");
                let cmd = new_pub_with_ttl(topic.to_string(), payload, default_ttl);
                let frame = encode_command_with_len_prefix(&cmd);
                w.write_all(&frame).await?;
            }

            ["sub", topic] => {
                let cmd = new_sub(topic.to_string());
                let frame = encode_command_with_len_prefix(&cmd);
                w.write_all(&frame).await?;
            }

            ["unsub", topic] => {
                let cmd = new_unsub(topic.to_string());
                let frame = encode_command_with_len_prefix(&cmd);
                w.write_all(&frame).await?;
            }

            _ => {
                println!("Unknown command (type `help`)");
            }
        }
    }

    drop(w); // shut down writer
    let _ = printer.await;
    Ok(())
}
