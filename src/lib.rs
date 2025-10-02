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
pub mod metrics;
pub mod util; // affinity utils, etc. // minimal global counters + export
// pub mod v2; // Ultra-high performance implementation - temporarily disabled

// Generated FlatBuffers bindings (from build.rs)
// We include them once here and allow lints that commonly trigger in generated code.
#[allow(
    dead_code,
    unused_imports,
    clippy::extra_unused_lifetimes,
    clippy::derivable_impls,
    clippy::duplicate_mod,
    clippy::missing_safety_doc
)]
pub mod generated {
    include!(concat!(env!("OUT_DIR"), "/flatbuffers/mod.rs"));
}

// ───────────────────────────────────────────────────────────
// Re-exports
// ───────────────────────────────────────────────────────────
pub use broker::engine::serve as start_broker;
pub use config::{load_config, Config};

// Optional global allocator
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// NOTE:
// The CLI (`blipmq-cli`) is now a standalone binary under `src/bin/`,
// so it’s no longer re-exported from this library.
