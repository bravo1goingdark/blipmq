
use blipmq::{config::load_config, logging::init_logging, run};
use std::process;

#[tokio::main]
async fn main() {
    init_logging();

    let config = match load_config("blipmq.toml") {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("[FATAL] Failed to load config: {e}");
            process::exit(1);
        }
    };

    if let Err(e) = run(config).await {
        eprintln!("[FATAL] Broker crashed: {e}");
        process::exit(1);
    }
}
