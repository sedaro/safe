use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Telemetry {
    pub timestamp: u64,
    pub proximity_m: i32,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Command {
    pub commanded_attitude: Vec<u8>,
    pub thrust: u8,
}