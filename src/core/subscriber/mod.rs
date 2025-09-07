//! BlipMQ Subscriber module.
//!
//! Provides the `Subscriber` struct and unique `SubscriberId` used in per-topic routing.

#[allow(clippy::module_inception)]
pub mod subscriber;

use bytes::{Bytes, BytesMut};
pub use subscriber::{Subscriber, SubscriberId};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::{sleep_until, Duration, Instant};

/// Spawn a dedicated writer task for a connection and return a sender for frames.
/// All socket writes for this connection MUST go through this channel.
/// This eliminates writer lock contention and ensures ordered writes.
pub fn spawn_connection_writer<W>(writer: W, channel_capacity: usize) -> mpsc::Sender<Bytes>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<Bytes>(channel_capacity);
    tokio::spawn(async move {
        use crate::config::CONFIG;
        let mut w = writer;
        let max_bytes: usize = CONFIG.delivery.max_batch_bytes.max(4096);
        let interval = Duration::from_millis(CONFIG.delivery.flush_interval_ms.max(1));

        let mut buf = BytesMut::with_capacity(max_bytes.min(64 * 1024));
        let mut deadline = Instant::now() + interval;

        loop {
            tokio::select! {
                maybe_frame = rx.recv() => {
                    match maybe_frame {
                        Some(frame) => {
                            buf.extend_from_slice(&frame);
                            if buf.len() >= max_bytes {
                                if let Err(e) = w.write_all(&buf).await {
                                    tracing::debug!("connection writer error: {:?}", e);
                                    break;
                                }
                                buf.clear();
                                deadline = Instant::now() + interval;
                            }
                        }
                        None => {
                            if !buf.is_empty() {
                                let _ = w.write_all(&buf).await;
                                buf.clear();
                            }
                            let _ = w.flush().await;
                            break;
                        }
                    }
                }
                _ = sleep_until(deadline), if !buf.is_empty() => {
                    if let Err(e) = w.write_all(&buf).await {
                        tracing::debug!("connection writer error: {:?}", e);
                        break;
                    }
                    buf.clear();
                    deadline = Instant::now() + interval;
                }
            }
        }
    });
    tx
}
