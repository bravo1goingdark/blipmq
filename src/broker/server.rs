use crate::config::Config;

pub async fn start_broker(config: Config) -> anyhow::Result<()> {
    tracing::info!("Broker starting on {}", config.server.bind_addr);
    // Here we'll later initialize WAL, queues, client handling etc.
    Ok(())
}
