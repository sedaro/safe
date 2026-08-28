use std::path::PathBuf;

use safe::protocol::{BoardCmdId, BoardState, TimedCommand};
use safe::telemetry_frame::TelemetryFrame;
use safe_sim::{EdsPatch, EdsPatchTarget, ProbabilityDistribution};
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
    /// Absolute tolerance used by `eq` and `ne`; ignored by ordered comparisons.
    #[serde(default = "default_comparison_tolerance")]
    pub tolerance: f64,
}

fn default_comparison_tolerance() -> f64 {
    1e-9
}

/// How a sampled scalar is combined with an adapter-provided baseline value.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonteCarloOperation {
    #[default]
    Replace,
    Add,
    Multiply,
}

/// Optional inclusive limits for the final value sent to EDS.
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct MonteCarloBounds {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

/// User-facing distribution configuration.
///
/// This tagged representation keeps `safe.yaml` readable while converting to
/// the shared `safe-sim` distribution used for sampling.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MonteCarloDistribution {
    Normal { mean: f64, std_dev: f64 },
    Uniform { low: f64, high: f64 },
    LogNormal { mean: f64, std_dev: f64 },
    Triangular { low: f64, high: f64, mode: f64 },
    Discrete { values: Vec<f64> },
}

impl From<&MonteCarloDistribution> for ProbabilityDistribution {
    fn from(value: &MonteCarloDistribution) -> Self {
        match value {
            MonteCarloDistribution::Normal { mean, std_dev } => Self::Normal {
                mean: *mean,
                std_dev: *std_dev,
            },
            MonteCarloDistribution::Uniform { low, high } => Self::Uniform {
                low: *low,
                high: *high,
            },
            MonteCarloDistribution::LogNormal { mean, std_dev } => Self::LogNormal {
                mean: *mean,
                std_dev: *std_dev,
            },
            MonteCarloDistribution::Triangular { low, high, mode } => Self::Triangular {
                low: *low,
                high: *high,
                mode: *mode,
            },
            MonteCarloDistribution::Discrete { values } => Self::Discrete {
                values: values.clone(),
            },
        }
    }
}

/// One independently sampled scalar EDS input.
#[derive(Clone, Debug, Deserialize)]
pub struct MonteCarloVariation {
    pub name: String,
    pub target: EdsPatchTarget,
    #[serde(default)]
    pub operation: MonteCarloOperation,
    pub distribution: MonteCarloDistribution,
    pub bounds: Option<MonteCarloBounds>,
}

/// Configuration for randomized simulations following the required nominal run.
#[derive(Clone, Debug, Deserialize)]
pub struct MonteCarloConfig {
    pub samples: usize,
    #[serde(default)]
    pub seed: u64,
    #[serde(default = "default_minimum_pass_fraction")]
    pub minimum_pass_fraction: f64,
    /// Maximum draws allowed when bounds reject sampled final values.
    #[serde(default = "default_max_resample_attempts")]
    pub max_resample_attempts: usize,
    pub variations: Vec<MonteCarloVariation>,
}

fn default_minimum_pass_fraction() -> f64 {
    1.0
}

fn default_max_resample_attempts() -> usize {
    1_000
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
    /// Optional wall-clock limit for each one-shot input adapter invocation.
    pub input_adapter_timeout_secs: Option<u64>,
    #[serde(default)]
    pub input_adapter_config: serde_json::Value,
    /// JSON Pointer to a finite GPS-seconds value in `TelemetryFrame.payload`.
    /// Required when evaluating an immediate command.
    pub telemetry_gps_time_pointer: Option<String>,
    /// Optional wall-clock limit applied separately to every EDS run.
    pub simulation_timeout_secs: Option<u64>,
    #[serde(default)]
    pub field_checks: Vec<FieldCheck>,
    /// Omit this section to run only the nominal simulation.
    pub monte_carlo: Option<MonteCarloConfig>,
}

impl Default for GatekeeperConfig {
    fn default() -> Self {
        Self {
            eds_path: PathBuf::default(),
            sim_duration_days: default_sim_duration_days(),
            input_adapter_command: Vec::new(),
            input_adapter_timeout_secs: None,
            input_adapter_config: serde_json::Value::Null,
            telemetry_gps_time_pointer: None,
            simulation_timeout_secs: None,
            field_checks: Vec::new(),
            monte_carlo: None,
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

/// One self-contained simulation scenario sent to the mission-specific input
/// adapter. The gatekeeper resolves SAFE board state before constructing it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationInputRequest {
    pub telemetry: TelemetryFrame,
    /// The complete command schedule to represent in this simulation, ordered
    /// by execution time. Commands with the same time retain proposal order.
    pub commands: Vec<TimedCommand>,
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
        assert!(config.input_adapter_timeout_secs.is_none());
        assert!(config.telemetry_gps_time_pointer.is_none());
        assert!(config.simulation_timeout_secs.is_none());
        assert!(config.field_checks.is_empty());
        assert!(config.monte_carlo.is_none());
    }

    #[test]
    fn monte_carlo_config_uses_tagged_distribution_and_safe_defaults() {
        let config: GatekeeperConfig = serde_json::from_value(serde_json::json!({
            "input_adapter_timeout_secs": 10,
            "telemetry_gps_time_pointer": "/spacecraft/gps_time",
            "simulation_timeout_secs": 300,
            "field_checks": [{
                "target_file": "output.jsonl",
                "field": "value",
                "op": "eq",
                "threshold": 1.0
            }],
            "monte_carlo": {
                "samples": 20,
                "variations": [{
                    "name": "soc_error",
                    "target": {
                        "agent_id": "agent",
                        "engine": "power",
                        "field": "battery.soc",
                        "type_": "f64"
                    },
                    "operation": "add",
                    "distribution": {
                        "kind": "normal",
                        "mean": 0.0,
                        "std_dev": 0.02
                    }
                }]
            }
        }))
        .unwrap();

        assert_eq!(config.input_adapter_timeout_secs, Some(10));
        assert_eq!(
            config.telemetry_gps_time_pointer.as_deref(),
            Some("/spacecraft/gps_time")
        );
        assert_eq!(config.simulation_timeout_secs, Some(300));
        assert_eq!(config.field_checks[0].tolerance, 1e-9);
        let monte_carlo = config.monte_carlo.unwrap();
        assert_eq!(monte_carlo.seed, 0);
        assert_eq!(monte_carlo.minimum_pass_fraction, 1.0);
        assert_eq!(monte_carlo.max_resample_attempts, 1_000);
        assert!(matches!(
            monte_carlo.variations[0].operation,
            MonteCarloOperation::Add
        ));
    }

    #[test]
    fn simulation_input_contains_resolved_command_schedule_not_board_state() {
        let request = SimulationInputRequest {
            telemetry: TelemetryFrame::new(serde_json::json!({"source": "test"})),
            commands: vec![
                TimedCommand::Now(safe::protocol::Command::PointNadir),
                TimedCommand::Now(safe::protocol::Command::PointSunYaw),
            ],
            config: serde_json::Value::Null,
        };

        let value = serde_json::to_value(request).unwrap();
        assert!(value["commands"].is_array());
        assert!(value.get("board").is_none());
        assert!(value.get("baseline_commands").is_none());
        assert!(value.get("candidate_command_ids").is_none());
    }
}
