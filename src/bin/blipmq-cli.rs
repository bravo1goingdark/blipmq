use crate::config::Config;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::TcpStream;

// handles a single client's tcp stream
pub async fn handle_client(
    stream: TcpStream,
    addr: SocketAddr,
    _config: Arc<Config>,
) -> anyhow::Result<()> {
    tracing::info!("Handling client from {}", addr);

    let peer: String = match stream.peer_addr() {
        Ok(addr) => addr.to_string(),
        Err(_) => "unknown".to_string(),
    };

    tracing::info!("Accepted connection from {}", peer);

    // split the tcp stream into read/write halves
    // handle each side independently
    // non-blocking full duplex
    let (reader, mut writer) = stream.into_split();

    // buffered line reader for efficient line-by-line protocol parsing
    let mut buf_reader: BufReader<OwnedReadHalf> = BufReader::new(reader);
    let mut line: String = String::new();

    loop {
        // enters a loop to read each incoming messages
        line.clear();

        // reads a single line -> until "\n" is hit from the client
        match buf_reader.read_line(&mut line).await {
            // read return 0 : reached EOF
            Ok(0) => {
                tracing::info!("Client {} disconnected", peer);
                return Ok(());
            }

            // logs the raw command received
            Ok(_) => {
                let trimmed = line.trim();
                tracing::info!("Received from {}: {}", peer, trimmed);

                // echoes ACK response
                // later we will route this to queue
                if let Err(e) = writer.write_all(b"ACK\n").await {
                    tracing::error!("Error writing to {}: {:?}", peer, e);
                    return Ok(());
                }
            }

            // reading fails -> connection reset / corrupted stream
            // will log the error and exit
            Err(e) => {
                tracing::error!("Error reading from {}: {:?}", peer, e);
                return Err(e.into());
            }
        }
    }
}
