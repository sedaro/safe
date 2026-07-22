use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::protocol::AutonomyModeId;
use crate::protocol::Command;
use crate::protocol::TimedCommand;
use crate::telemetry_frame::TelemetryFrame;

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeConfigView {
    pub logging: RuntimeLoggingConfig,
    pub base_paths: RuntimeBasePathsConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeLoggingConfig {
    pub file_path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeBasePathsConfig {
    pub base_writable_directory: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomyModeConfigItem {
    pub name: String,
    pub priority: u8,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FlightCheckpoint {
    pub active_autonomy_mode: Option<AutonomyModeId>,
    pub autonomy_modes: Vec<AutonomyModeMeta>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AutonomyModeMeta {
    pub id: AutonomyModeId,
    pub priority: u8,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessResourceSnapshot {
    pub pid: u32,
    #[serde(default)]
    pub parent_pid: Option<u32>,
    #[serde(default)]
    pub command: String,
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub disk_read_bytes: u64,
    pub disk_written_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeResourceSnapshot {
    pub mode_id: AutonomyModeId,
    pub timestamp_unix_ms: u64,
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub disk_read_bytes: u64,
    pub disk_written_bytes: u64,
    pub process_count: u32,
    pub min_cpu_30_min: f64,
    pub max_cpu_30_min: f64,
    pub avg_cpu_30_min: f64,
    pub min_memory_30_min: u64,
    pub max_memory_30_min: u64,
    pub avg_memory_30_min: u64,
    #[serde(default)]
    pub processes: Vec<ProcessResourceSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostCommandRequest {
    pub request_id: String,
    pub command: ExternalCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostCommandStatusState {
    Received,
    Accepted,
    Rejected,
    Dispatched,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostCommandStatus {
    pub request_id: String,
    pub state: HostCommandStatusState,
    pub detail: String,
    pub ts_mono: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostCommandDispatchRecord {
    pub event_seq: u64,
    pub event_ts_mono: u64,
    pub event_source: String,
    pub event_msg_kind: String,
    pub timed_command: TimedCommand,
}

pub fn mode_id_from_name(name: &str) -> AutonomyModeId {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, name.as_bytes()).into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExternalCommand {
    ExecuteNow { command: Command },
    ActivateMode { mode: AutonomyModeId },
    DeactivateMode { mode: AutonomyModeId },
    RestartMode { mode: AutonomyModeId },
    StopMode { mode: AutonomyModeId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SafectlIngress {
    Command {
        command: ExternalCommand,
        #[serde(default)]
        request_id: Option<String>,
    },
    Telemetry {
        telemetry: TelemetryFrame,
    },
}
