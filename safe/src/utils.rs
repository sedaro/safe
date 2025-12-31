use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Timestamped<W> {
    timestamp: u128,
    wrapped: W,
}
impl<W> Timestamped<W> {
    pub fn new(wrapped: W) -> Self {
        Self {
            timestamp: SystemTime::now()
              .duration_since(UNIX_EPOCH)
              .expect("Time went backwards")
              .as_nanos(),
            wrapped,
        }
    }
    pub fn timestamp(&self) -> u128 {
        self.timestamp
    }
    pub fn into_inner(self) -> W {
        self.wrapped
    }
    pub fn inner(&self) -> &W {
        &self.wrapped
    }
}