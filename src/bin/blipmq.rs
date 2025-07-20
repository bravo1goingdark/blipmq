//! blipmq – one binary that can start the broker *or* act as an
//! interactive client shell.
//
//  $ blipmq start --config blipmq.toml
//  $ blipmq connect 127.0.0.1:6379
//  > pub news hello
//  > sub news
//  MSG … hello
//  > exit

use blipmq::{load_config, start_broker, Config};
use clap::{Parser, Subcommand};
use rustyline::history::DefaultHistory;
use rustyline::{DefaultEditor, Editor};
use std::net::SocketAddr;
use tokio::task::JoinHandle;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
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
            let cfg_path : String = std::env::var("BLIPMQ_CONFIG").unwrap_or(config);
            let cfg : Config = load_config(&cfg_path)?;
            println!("📡 BlipMQ broker listening on {}", cfg.server.bind_addr);
            start_broker(&cfg.server.bind_addr).await?;
        }
        Command::Connect { addr } => repl(addr).await?,
    }
    Ok(())
}

// ───────────────────────────────────────────────────────────
// Interactive REPL shell
// ───────────────────────────────────────────────────────────
// ───────────────────────────────────────────────────────────
// Interactive REPL shell (real-time printing)
// ───────────────────────────────────────────────────────────
async fn repl(addr: SocketAddr) -> anyhow::Result<()> {
    // Rusty line editor for history & prompt
    let mut rl: Editor<(), DefaultHistory> = DefaultEditor::new()?;

    // Connect to broker
    let stream: TcpStream = TcpStream::connect(addr).await?;
    let (r, mut w) = stream.into_split();
    let reader = BufReader::new(r).lines();

    println!("Connected to {addr}. Type `help` for commands.");

    let printer: JoinHandle<()> = tokio::spawn(async move {
        let mut lines = reader;
        while let Ok(Some(line)) = lines.next_line().await {
            println!("{line}");
        }
    });

    loop {
        let Ok(line) = rl.readline("> ") else { break };
        let _ = rl.add_history_entry(line.as_str());

        match line.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["help"] => println!("pub <topic> <msg> | sub <topic> | unsub <topic> | exit"),
            ["exit" | "quit"] => break,
            ["pub", topic, rest @ ..] => {
                w.write_all(format!("PUB {topic} {}\n", rest.join(" ")).as_bytes())
                    .await?;
            }
            ["sub", topic] => w.write_all(format!("SUB {topic}\n").as_bytes()).await?,
            ["unsub", topic] => w.write_all(format!("UNSUB {topic}\n").as_bytes()).await?,
            _ => println!("Unknown cmd. Type `help`."),
        }
    }

    drop(w);
    let _ = printer.await;
    Ok(())
}
