/// QoS 0 delivery mode — controls message ordering and throughput tradeoffs.
#[derive(Debug, Clone, Copy)]
pub enum DeliveryMode {
    /// Guarantees in-order delivery (default).
    Ordered,
    /// Allows parallel fan-out; breaks ordering but boosts throughput.
    Unordered,
}
