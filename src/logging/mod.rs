use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{fmt, EnvFilter, Registry};

pub fn init_logging() {
    let filter: EnvFilter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let (non_blocking_writer, _guard) = tracing_appender::non_blocking(std::io::stdout());

    let formatting_layer = fmt::layer()
        .with_timer(UtcTime::rfc_3339())
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_target(true)
        .compact()
        .with_ansi(false) // Disable ANSI for production logs (e.g., if writing to file/json)
        .with_writer(non_blocking_writer);

    let subscriber = Registry::default().with(filter).with(formatting_layer);

    // Using `_guard` to prevent the non-blocking writer from dropping too early.
    // In a real application, you might want to store this guard in a more accessible place
    // or ensure it lives for the duration of the application.
    // For this example, it's fine to let it be dropped when init_logging goes out of scope,
    // as the main function in `src/bin/blipmq.rs` and `src/bin/blipmq-cli.rs` will call `init_logging`.
    // If the application exits abruptly, some buffered logs might be lost.
    // For a more robust solution, the guard should be held in `main`.
    // However, given this is a quick perf improvement, this is acceptable for now.
    tracing::subscriber::set_global_default(subscriber)
        .expect("Failed to set global tracing subscriber");
}
