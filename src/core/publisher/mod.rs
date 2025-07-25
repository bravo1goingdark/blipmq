//! Publisher module for blipmq
//! Exposes the Publisher interface for message publication.
#[allow(clippy::module_inception)]
pub mod publisher;
pub use publisher::PublisherConfig;

pub use publisher::Publisher;
