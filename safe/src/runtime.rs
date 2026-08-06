use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::protocol::AutonomyModeId;
use crate::protocol::BoardCmdId;
use crate::protocol::Command;
use crate::protocol::TimedCommand;
use crate::telemetry_frame::TelemetryFrame;

pub const DEFAULT_LOG_STREAM: &str = "safe";

/// Stable record format used by SAFE's per-scope operational log files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogRecord {
    pub timestamp: String,
    pub level: String,
    pub target: String,
    #[serde(default)]
    pub mode_id: Option<Uuid>,
    #[serde(default = "default_log_stream")]
    pub stream: String,
    pub message: String,
    #[serde(default)]
    pub fields: BTreeMap<String, Value>,
}

fn default_log_stream() -> String {
    DEFAULT_LOG_STREAM.to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeConfigView {
    #[serde(default)]
    pub tracing: RuntimeTracingConfig,
    pub logging: RuntimeLoggingConfig,
    pub base_paths: RuntimeBasePathsConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeTracingConfig {
    #[serde(default = "default_true")]
    pub with_target: bool,
}

impl Default for RuntimeTracingConfig {
    fn default() -> Self {
        Self { with_target: true }
    }
}

fn default_true() -> bool {
    true
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FlightCheckpoint {
    pub active_autonomy_mode: Option<AutonomyModeId>,
    pub autonomy_modes: Vec<AutonomyModeMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomyModeMeta {
    pub id: AutonomyModeId,
    #[serde(default)]
    pub name: String,
    pub priority: u8,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeConnectionState {
    NotStarted,
    Starting,
    Connecting,
    Connected,
    Disconnected,
    Faulted,
    Stopped,
    Unresponsive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeHandlerState {
    Ready,
    Active,
    Inactive,
    Stopping,
    Faulted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeRuntimeStatus {
    pub connection: ModeConnectionState,
    #[serde(default)]
    pub handler: Option<ModeHandlerState>,
    #[serde(default)]
    pub last_transition_unix_ms: Option<u64>,
    #[serde(default)]
    pub last_heartbeat_unix_ms: Option<u64>,
    #[serde(default)]
    pub detail: Option<String>,
}

impl Default for ModeRuntimeStatus {
    fn default() -> Self {
        Self {
            connection: ModeConnectionState::NotStarted,
            handler: None,
            last_transition_unix_ms: None,
            last_heartbeat_unix_ms: None,
            detail: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationalStatus {
    pub schema_version: u16,
    pub updated_at: String,
    pub daemon: DaemonStatus,
    pub telemetry: TelemetryStatus,
    pub board: Vec<BoardCommandStatus>,
    pub modes: Vec<ModeOperationalStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: u32,
    pub running: bool,
    pub halted: bool,
    #[serde(default)]
    pub fault: Option<String>,
    pub last_seq_applied: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelemetryStatus {
    pub received_count: u128,
    #[serde(default)]
    pub last_received_at: Option<String>,
    #[serde(default)]
    pub latest: Option<TelemetryStatusFrame>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryStatusFrame {
    #[serde(default)]
    pub source: Option<String>,
    pub ts_mono: u64,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardCommandState {
    Pending,
    Approved,
    Rejected,
    Published,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardCommandStatus {
    pub id: BoardCmdId,
    pub from: AutonomyModeId,
    pub command: TimedCommand,
    pub proposed_ts_mono: u64,
    pub state: BoardCommandState,
    #[serde(default)]
    pub decision_by: Option<String>,
    #[serde(default)]
    pub decision_reason: Option<String>,
    #[serde(default)]
    pub decision_ts_mono: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeOperationalStatus {
    pub name: String,
    pub id: AutonomyModeId,
    pub priority: u8,
    pub enabled: bool,
    pub eligible: bool,
    pub active: bool,
    pub manual_override: bool,
    pub selection_reason: String,
    pub eligibility_reason: String,
    pub runtime: ModeRuntimeStatus,
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
