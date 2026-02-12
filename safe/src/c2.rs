
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Telemetry {
    // pub timestamp: u64,
    pub pointing_error: f64,
    pub in_sunlight: bool,
    pub disk_util: f64,
    pub battery_soc: f64,
    pub od_solution: (f64, f64, f64),
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub enum Command {
    SetPidControllerGains(f64, f64, f64, f64),
    IridiumPowerOn,
    IridiumPowerOff,
    IridiumTransmitMsg(String),
    PointSunYaw,
    PointNadir,
    CaptureImage,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub enum TimedCommand {
    Now(Command),
    Scheduled { cmd: Command, gps_time: f64 },
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