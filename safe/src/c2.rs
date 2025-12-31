use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Telemetry {
    // pub timestamp: u64,
    pub pointing_error: f64,
    pub in_sunlight: bool,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Command {
    pub set_pid_controller_gains: (f64, f64, f64, f64),
    timestamp_ns: u128,
}

impl Command {
    pub fn new(set_pid_controller_gains: (f64, f64, f64, f64)) -> Self {
        Self {
            set_pid_controller_gains,
            timestamp_ns: SystemTime::now()
              .duration_since(UNIX_EPOCH)
              .expect("Time went backwards")
              .as_nanos(),
        }
    }
}

// Temporary work around for command queue flushes (FIXME)
pub trait Timestamped {
    fn unix_timestamp_ns(&self) -> u128;
}

impl Timestamped for Command {
    fn unix_timestamp_ns(&self) -> u128 {
        self.timestamp_ns
    }
}

// CLI request structures

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct LogsRequest {
    pub mode: Option<String>,
    pub level: Option<String>,
    pub follow: bool,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct CommandsRequest {}