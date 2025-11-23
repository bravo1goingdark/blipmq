use thiserror::Error;

use crate::frame::{FrameDecodeError, FrameEncodeError};

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("frame decode error: {0}")]
    FrameDecode(#[from] FrameDecodeError),

    #[error("frame encode error: {0}")]
    FrameEncode(#[from] FrameEncodeError),

    #[error("handler error: {0}")]
    Handler(String),
}
