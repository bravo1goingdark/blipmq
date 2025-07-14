pub mod config;
pub mod logging;
pub mod broker;

use crate::broker::server::start_broker;
use crate::config::Config;

pub async fn run(config: Config) -> anyhow::Result<()> {
    start_broker(config).await
}
