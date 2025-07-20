//! # Broker Module
//!
//! This module provides the top-level Broker abstraction and the CLI client
//! for interacting with a running BlipMQ broker instance.
//!
//! - `client`: A command-line interface (CLI) tool for publishing, subscribing,
//!   and unsubscribing.
//! - `engine`: The server implementation powering the broker, handling TCP connections,
//!   dispatching commands, and integrating with the core engine.
//!
//! **Entry Point:**
//! This file (`mod.rs`) serves as the single entrypoint for all future CLI commands
//! via the `BrokerCli` re-export.

pub mod engine;

/// Re-export of the `serve` function to start the broker server.
///
/// # Example
///
/// ```bash
/// blipmq-broker --addr 127.0.0.1:6379
/// ```
pub use self::engine::serve;
pub use self::engine::server;


