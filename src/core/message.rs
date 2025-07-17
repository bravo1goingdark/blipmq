#[derive(Debug, Clone)]
pub struct Message {
    pub id: String,
    pub payload: Vec<u8>,
    pub timestamp: u64,
}

impl Message {
    pub fn new(id: impl Into<String>, payload: impl Into<Vec<u8>>, timestamp: u64) -> Self {
        self {
            id: id.into(),
            payload: payload.into(),
            timestamp,
        }
    }
}
