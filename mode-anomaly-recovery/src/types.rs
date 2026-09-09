use safe_sim::CancellationToken;
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64},
};

use safe::protocol::AutonomyModeBoardState;
use serde::Serialize;
use serde_json::Value;

use crate::config::{AllowedAction, AnomalyRecoveryModeConfig, AnomalySeverity};

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

pub(crate) struct AnomalyRecoveryMode {
    pub(crate) config: AnomalyRecoveryModeConfig,
    pub(crate) latest_telemetry: Option<TelemetrySample>,
    pub(crate) current_candidates: Vec<AnomalyCandidate>,
    pub(crate) rule_states: HashMap<String, RuleState>,
    pub(crate) latest_board_snapshot: AutonomyModeBoardState,
    pub(crate) has_board_snapshot: bool,
    pub(crate) last_plan_signature: Option<String>,
    pub(crate) warned_missing_board_snapshot: bool,
    pub(crate) planning_generation: Arc<AtomicU64>,
    pub(crate) active: Arc<AtomicBool>,
    pub(crate) planning_cancel: Option<CancellationToken>,
}

impl AnomalyRecoveryMode {
    pub(crate) fn new() -> Self {
        Self {
            config: AnomalyRecoveryModeConfig::default(),
            latest_telemetry: None,
            current_candidates: Vec::new(),
            rule_states: HashMap::new(),
            latest_board_snapshot: AutonomyModeBoardState::default(),
            has_board_snapshot: false,
            last_plan_signature: None,
            warned_missing_board_snapshot: false,
            planning_generation: Arc::new(AtomicU64::new(0)),
            active: Arc::new(AtomicBool::new(false)),
            planning_cancel: None,
        }
    }
}
