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
}

impl Command {
    pub fn new(set_pid_controller_gains: (f64, f64, f64, f64)) -> Self {
        Self {
            set_pid_controller_gains,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum AutonomyModeMessage<T> {
    Telemetry(T),
    Active { nonce: u64, },
    Inactive,
    // InactiveWarning, // TODO: Warn of getting unscheduled
    // C2AcceptedCommand { generation: u64, seq: u64,  }, // TODO: Ack from C2 that command was accepted
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum RouterMessage<C> {
    Command { data: C, nonce: u64, },
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