//! Server engine module for BlipMQ broker.
//!
//! Exposes the `serve` function which starts the broker's TCP listener
//! and dispatches client commands to the core pub/sub engine.

pub mod server;

/// Starts the BlipMQ broker server.
///
/// # Example
///
/// ```bash
/// blipmq-broker --addr 127.0.0.1:6379
/// ```
pub use server::serve;
