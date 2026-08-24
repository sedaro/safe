use std::collections::HashMap;

use safe::protocol::AutonomyModeBoardState;
use serde::Serialize;
use serde_json::Value;

use crate::config::{AllowedAction, AnomalySeverity, LlmAdvisorModeConfig};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TelemetrySample {
    pub(crate) source: Option<String>,
    pub(crate) ts_mono: u64,
    pub(crate) payload: Value,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AnomalyCandidate {
    pub(crate) profile_id: String,
    pub(crate) rule_id: String,
    pub(crate) anomaly_id: String,
    pub(crate) source: String,
    pub(crate) ts_mono: u64,
    pub(crate) path: String,
    pub(crate) observed: Value,
    pub(crate) expectation: String,
    pub(crate) severity: AnomalySeverity,
    pub(crate) eligible_actions: Vec<AllowedAction>,
}

#[derive(Debug, Default)]
pub(crate) struct RuleState {
    pub(crate) consecutive_violations: usize,
}

pub(crate) struct LlmAdvisorMode {
    pub(crate) config: LlmAdvisorModeConfig,
    pub(crate) latest_telemetry: Option<TelemetrySample>,
    pub(crate) current_candidates: Vec<AnomalyCandidate>,
    pub(crate) rule_states: HashMap<String, RuleState>,
    pub(crate) latest_board_snapshot: AutonomyModeBoardState,
    pub(crate) has_board_snapshot: bool,
    pub(crate) last_plan_signature: Option<String>,
    pub(crate) warned_missing_board_snapshot: bool,
}

impl LlmAdvisorMode {
    pub(crate) fn new() -> Self {
        Self {
            config: LlmAdvisorModeConfig::default(),
            latest_telemetry: None,
            current_candidates: Vec::new(),
            rule_states: HashMap::new(),
            latest_board_snapshot: AutonomyModeBoardState::default(),
            has_board_snapshot: false,
            last_plan_signature: None,
            warned_missing_board_snapshot: false,
        }
    }
}
