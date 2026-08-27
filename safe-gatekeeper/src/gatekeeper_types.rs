use std::path::PathBuf;

use safe::protocol::{BoardCmdId, BoardState};
use safe::telemetry_frame::TelemetryFrame;
use safe_sim::EdsPatch;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckAggregation {
    #[default]
    Last,
    Min,
    Max,
    Mean,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonOp {
    Lt,
    Lte,
    Gt,
    Gte,
    Eq,
    Ne,
}

/// A generic numeric constraint over one field in an EDS output file.
#[derive(Clone, Debug, Deserialize)]
pub struct FieldCheck {
    pub target_file: String,
    pub field: String,
    #[serde(default)]
    pub aggregation: CheckAggregation,
    pub op: ComparisonOp,
    pub threshold: f64,
}

/// Static gatekeeper settings supplied through `safe.yaml`.
///
/// The adapter command contains the executable as its first item followed by
/// its arguments, for example `["/opt/bin/mission-input-adapter", "gatekeeper-input"]`.
#[derive(Clone, Debug, Deserialize)]
pub struct GatekeeperConfig {
    #[serde(default)]
    pub eds_path: PathBuf,
    #[serde(default = "default_sim_duration_days")]
    pub sim_duration_days: f64,
    #[serde(default)]
    pub input_adapter_command: Vec<String>,
    #[serde(default)]
    pub input_adapter_config: serde_json::Value,
    #[serde(default)]
    pub field_checks: Vec<FieldCheck>,
}

impl Default for GatekeeperConfig {
    fn default() -> Self {
        Self {
            eds_path: PathBuf::default(),
            sim_duration_days: default_sim_duration_days(),
            input_adapter_command: Vec::new(),
            input_adapter_config: serde_json::Value::Null,
            field_checks: Vec::new(),
        }
    }
}

fn default_sim_duration_days() -> f64 {
    1.0
}

/// Messages SAFE sends to the gatekeeper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GatekeeperInput {
    Telemetry(TelemetryFrame),
    EvaluateBatch {
        request_id: u64,
        board: BoardState,
        candidate_command_ids: Vec<BoardCmdId>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GatekeeperOutput {
    Approve { request_id: u64, details: String },
    Reject { request_id: u64, reason: String },
}

/// One self-contained request sent to the mission-specific input adapter.
/// The gatekeeper deliberately treats telemetry and commands as opaque data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationInputRequest {
    pub telemetry: TelemetryFrame,
    pub board: BoardState,
    pub candidate_command_ids: Vec<BoardCmdId>,
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Materialized inputs required to initialize an arbitrary SCF EDS run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationInputResponse {
    pub start_time_mjd: f64,
    pub patches: Vec<EdsPatch>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_contain_no_mission_specific_values() {
        let config = GatekeeperConfig::default();
        assert!(config.eds_path.as_os_str().is_empty());
        assert!(config.input_adapter_command.is_empty());
        assert!(config.field_checks.is_empty());
    }
}
