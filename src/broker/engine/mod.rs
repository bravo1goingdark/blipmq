//! Server engine module for BlipMQ broker.
//!
//! Exposes both the legacy `serve` function and the new production-optimized
//! server implementation with advanced performance features.

pub mod server;
pub mod optimized_server;

/// Starts the legacy BlipMQ broker server.
pub use server::serve;

/// Starts the production-optimized BlipMQ broker server.
pub use optimized_server::serve_optimized;
