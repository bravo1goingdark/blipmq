//! BlipMQ – A lightweight, high-throughput message broker written in Rust.
//!
//! This crate exports
//!  * `core`   – low-level message, queue, topic logic
//!  * `broker` – TCP server-side engine
//!  * `config` – TOML-driven runtime configuration
//!
//! Downstream applications can embed the broker engine (`start_broker`) or
//! build their own binaries on top of the library.

// ───────────────────────────────────────────────────────────
// Public modules
// ───────────────────────────────────────────────────────────
pub mod broker;
pub mod config;
pub mod core;
pub mod logging;

// ───────────────────────────────────────────────────────────
// Re-exports
// ───────────────────────────────────────────────────────────
pub use broker::engine::serve as start_broker;
pub use config::{load_config, Config};

// NOTE:
// The CLI (`blipmq-cli`) is now a standalone binary under `src/bin/`,
// so it’s no longer re-exported from this library.
