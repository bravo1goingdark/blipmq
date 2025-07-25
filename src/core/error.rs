use std::fmt;

#[derive(Debug)]
pub enum BlipError {
    Disconnected,
    QueueClosed,
    QueueFull,
    InvalidSubscriber,
    Timeout,
    Internal(String), // for any custom internal errors
}

impl std::error::Error for BlipError {}

impl fmt::Display for BlipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlipError::Disconnected => write!(f, "Subscriber is disconnected"),
            BlipError::QueueClosed => write!(f, "Queue is closed"),
            BlipError::QueueFull => write!(f, "Queue is full"),
            BlipError::InvalidSubscriber => write!(f, "Invalid subscriber"),
            BlipError::Timeout => write!(f, "Operation timed out"),
            BlipError::Internal(msg) => write!(f, "Internal error: {msg}"),
        }
    }
}
