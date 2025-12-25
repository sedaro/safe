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


// CLI request structures

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct LogsRequest {
    pub mode: Option<String>,
    pub level: Option<String>,
    pub follow: bool,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct CommandsRequest {}