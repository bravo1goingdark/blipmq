//! BlipMQ Subscriber module.
//!
//! Provides the `Subscriber` struct and unique `SubscriberId` used in per-topic routing.

#[allow(clippy::module_inception)]
pub mod subscriber;

pub use subscriber::{Subscriber, SubscriberId};
