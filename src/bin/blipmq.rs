//! blipmq – one binary that can start the broker *or* act as an
//! interactive client shell.
//
//  $ blipmq start --config blipmq.toml
//  $ blipmq connect 127.0.0.1:8080
//  > pub chat 5000 hello everyone  # custom TTL 5000ms
//  > pub chat hello everyone       # default TTL from config
//  > sub chat
//  > unsub chat
//  > exit

use blipmq::config::CONFIG;
use blipmq::core::command::{encode_command, new_pub_with_ttl, new_sub, new_unsub};
use blipmq::core::message::decode_message;
use blipmq::{load_config, start_broker};

use clap::{Parser, Subcommand};
use rustyline::history::DefaultHistory;
use rustyline::{DefaultEditor, Editor};

use std::net::SocketAddr;
use tokio::task::JoinHandle;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
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
            let cfg_path: String = std::env::var("BLIPMQ_CONFIG").unwrap_or(config);
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

    // Connect to broker
    let stream = TcpStream::connect(addr).await?;
    let (r, mut w) = stream.into_split();
    let mut raw_reader = BufReader::new(r);

    // Pull default TTL from config
    let default_ttl = CONFIG.delivery.default_ttl_ms;

    println!("Connected to {addr}. Type `help` for commands.");

    // Spawn a task to print responses and inbound messages
    let printer: JoinHandle<()> = tokio::spawn(async move {
        loop {
            let mut first = [0u8; 1];
            if raw_reader.read_exact(&mut first).await.is_err() {
                break;
            }

            if first[0].is_ascii_graphic() || first[0] == b' ' {
                // ASCII response
                let mut line = Vec::from(first);
                if raw_reader.read_until(b'\n', &mut line).await.is_err() {
                    break;
                }
                if let Ok(text) = std::str::from_utf8(&line) {
                    println!("> {}", text.trim_end());
                }
            } else {
                // Binary message
                let mut len_buf = [0u8; 4];
                len_buf[0] = first[0];
                if raw_reader.read_exact(&mut len_buf[1..]).await.is_err() {
                    break;
                }
                let msg_len = u32::from_be_bytes(len_buf) as usize;
                let mut msg_buf = vec![0u8; msg_len];
                if raw_reader.read_exact(&mut msg_buf).await.is_err() {
                    break;
                }
                match decode_message(&msg_buf) {
                    Ok(msg) => {
                        let payload = String::from_utf8_lossy(&msg.payload);
                        println!("{} {} @{}", msg.id, payload, msg.timestamp);
                    }
                    Err(e) => {
                        eprintln!("❌ Decode error: {}", e);
                    }
                }
            }
        }
    });

    // REPL loop
    while let Ok(line) = rl.readline("> ") {
        rl.add_history_entry(line.as_str())?;

        let parts: Vec<_> = line.split_whitespace().collect();
        match parts.as_slice() {
            ["help"] => {
                println!("Commands:\n  pub <topic> [ttl_ms] <msg...>\n  sub <topic>\n  unsub <topic>\n  exit");
            }

            ["exit" | "quit"] => break,

            // custom TTL: three+ words
            ["pub", topic, ttl_str, rest @ ..]
                if ttl_str.parse::<u64>().is_ok() && !rest.is_empty() =>
            {
                let ttl = ttl_str.parse()?;
                let payload = rest.join(" ");
                let cmd = new_pub_with_ttl(topic.to_string(), payload, ttl);
                let enc = encode_command(&cmd);
                w.write_all(&(enc.len() as u32).to_be_bytes()).await?;
                w.write_all(&enc).await?;
                w.flush().await?;
            }

            // default TTL
            ["pub", topic, rest @ ..] if !rest.is_empty() => {
                let payload = rest.join(" ");
                let cmd = new_pub_with_ttl(topic.to_string(), payload, default_ttl);
                let enc = encode_command(&cmd);
                w.write_all(&(enc.len() as u32).to_be_bytes()).await?;
                w.write_all(&enc).await?;
                w.flush().await?;
            }

            ["sub", topic] => {
                let cmd = new_sub(topic.to_string());
                let enc = encode_command(&cmd);
                w.write_all(&(enc.len() as u32).to_be_bytes()).await?;
                w.write_all(&enc).await?;
                w.flush().await?;
            }

            ["unsub", topic] => {
                let cmd = new_unsub(topic.to_string());
                let enc = encode_command(&cmd);
                w.write_all(&(enc.len() as u32).to_be_bytes()).await?;
                w.write_all(&enc).await?;
                w.flush().await?;
            }

            _ => {
                println!("Unknown command (type `help`)");
            }
        }
    }

    drop(w);
    let _ = printer.await;
    Ok(())
}
