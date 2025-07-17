use crate::config::Config;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::task;

// entry point to start the broker
// accepts a config object -> helps us avoid hardcoding values
pub async fn start_broker(config: Config) -> anyhow::Result<()> {
    tracing::info!("Broker starting on {}", config.server.bind_addr);
    // Here we'll later initialize WAL, queues, client handling etc.

    // bind to the host:port specified in blipmq.toml
    let listener: TcpListener = TcpListener::bind(&config.server.bind_addr).await?;
    tracing::info!("Broker bound to {}", config.server.bind_addr);

    // make config clonable for tasks
    let config: Arc<Config> = Arc::new(config);

    loop {
        // accepts incoming tcp connection's
        let (stream, addr) = listener.accept().await?;
        tracing::info!("Accepted connection from {}", addr);

        let config : Arc<Config> = Arc::clone(&config);
        task::spawn(async move {
            if let Err(e) = handle_client(stream,addr,config).await {
                tracing::error!("Error handling client {} : {:?}" , addr , e);
            }

        });

        drop(stream); // drops the tcp connection
    }
    Ok(())
}
