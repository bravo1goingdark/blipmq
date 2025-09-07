use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// Global counters (low overhead). These are coarse-grained and process-wide.
static PUBLISHED: AtomicU64 = AtomicU64::new(0);
static ENQUEUED: AtomicU64 = AtomicU64::new(0);
static DROPPED_TTL: AtomicU64 = AtomicU64::new(0);
static DROPPED_SUB_Q_FULL: AtomicU64 = AtomicU64::new(0);
static FLUSH_BYTES: AtomicU64 = AtomicU64::new(0);
static FLUSH_BATCHES: AtomicU64 = AtomicU64::new(0);

// Broker readiness state
static READY: AtomicBool = AtomicBool::new(false);

#[inline]
pub fn set_ready(v: bool) {
    READY.store(v, Ordering::Relaxed);
}

#[inline]
pub fn is_ready() -> bool {
    READY.load(Ordering::Relaxed)
}

#[inline]
pub fn inc_published(n: u64) {
    PUBLISHED.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn inc_enqueued(n: u64) {
    ENQUEUED.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn inc_dropped_ttl(n: u64) {
    DROPPED_TTL.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn inc_dropped_sub_queue_full(n: u64) {
    DROPPED_SUB_Q_FULL.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn inc_flush_bytes(n: u64) {
    FLUSH_BYTES.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn inc_flush_batches(n: u64) {
    FLUSH_BATCHES.fetch_add(n, Ordering::Relaxed);
}

pub fn snapshot() -> String {
    // Simple text format (Prometheus-style without HELP/TYPE lines for brevity)
    format!(
        "blipmq_published {}\nblipmq_enqueued {}\nblipmq_dropped_ttl {}\nblipmq_dropped_sub_queue_full {}\nblipmq_flush_bytes {}\nblipmq_flush_batches {}\n",
        PUBLISHED.load(Ordering::Relaxed),
        ENQUEUED.load(Ordering::Relaxed),
        DROPPED_TTL.load(Ordering::Relaxed),
        DROPPED_SUB_Q_FULL.load(Ordering::Relaxed),
        FLUSH_BYTES.load(Ordering::Relaxed),
        FLUSH_BATCHES.load(Ordering::Relaxed),
    )
}
