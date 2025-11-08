//! Adaptive spin-yield backoff utilities optimized for async runtimes.
//!
//! These helpers provide a lightweight way to apply pressure-aware backoff
//! without immediately yielding to the scheduler. They are ideal when
//! interacting with lock-free data structures where transient contention is
//! expected.

use tokio::task::yield_now;

/// Adaptive backoff that starts with CPU spins before yielding to the runtime.
#[derive(Debug, Default)]
pub struct AdaptiveYield {
    spins: u32,
}

impl AdaptiveYield {
    /// Creates a new adaptive backoff helper.
    pub const fn new() -> Self {
        Self { spins: 0 }
    }

    /// Perform the next backoff step.
    ///
    /// The strategy is:
    /// - For the first few invocations, spin with exponential backoff.
    /// - After the spin budget is exhausted, yield to the async scheduler.
    pub async fn snooze(&mut self) {
        if self.spins < 6 {
            let spins = 1 << self.spins;
            for _ in 0..spins {
                std::hint::spin_loop();
            }
            self.spins += 1;
        } else {
            yield_now().await;
            self.spins = 0;
        }
    }
}
