use safe_sim::CancellationToken;
use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, AtomicU64},
};

use safe::protocol::AutonomyModeBoardState;
use safe_llm_adapter::{AdapterRegistry, LlmAdapter};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{AllowedAction, AnomalyRecoveryModeConfig, AnomalySeverity};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TelemetrySample {
    pub(crate) source: Option<String>,
    pub(crate) ts_mono: u64,
    pub(crate) payload: Value,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct LiveContextSnapshot {
    pub(crate) telemetry: Option<TelemetrySample>,
    pub(crate) board: Option<AutonomyModeBoardState>,
    pub(crate) telemetry_version: u64,
    pub(crate) board_version: u64,
    pub(crate) telemetry_history: HashMap<String, VecDeque<TelemetrySample>>,
}

#[derive(Clone, Default)]
pub(crate) struct LiveContext {
    snapshot: Arc<RwLock<LiveContextSnapshot>>,
}

impl LiveContext {
    pub(crate) fn clear(&self) {
        *self
            .snapshot
            .write()
            .unwrap_or_else(|error| error.into_inner()) = LiveContextSnapshot::default();
    }

    pub(crate) fn update_telemetry(&self, telemetry: TelemetrySample) {
        let mut snapshot = self
            .snapshot
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(source) = telemetry.source.clone() {
            let history = snapshot.telemetry_history.entry(source).or_default();
            if history
                .back()
                .is_none_or(|previous| previous.ts_mono < telemetry.ts_mono)
            {
                history.push_back(telemetry.clone());
                while history.len() > 8 {
                    history.pop_front();
                }
            }
        }
        snapshot.telemetry = Some(telemetry);
        snapshot.telemetry_version = snapshot.telemetry_version.saturating_add(1);
    }

    pub(crate) fn update_board(&self, board: AutonomyModeBoardState) {
        let mut snapshot = self
            .snapshot
            .write()
            .unwrap_or_else(|error| error.into_inner());
        snapshot.board = Some(board);
        snapshot.board_version = snapshot.board_version.saturating_add(1);
    }

    pub(crate) fn snapshot(&self) -> LiveContextSnapshot {
        self.snapshot
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AssessmentOutcome {
    ThermalAnomaly,
    NoThermalAnomaly,
    Inconclusive,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecoveryDisposition {
    Monitor,
    OperatorReview,
    EvaluateRecovery,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ThermalAssessment {
    pub(crate) episode_id: String,
    pub(crate) revision: u64,
    pub(crate) outcome: AssessmentOutcome,
    pub(crate) disposition: RecoveryDisposition,
    pub(crate) candidate_ids: Vec<String>,
    pub(crate) evidence_ids: Vec<String>,
    pub(crate) rationale: String,
    pub(crate) uncertainty: String,
    pub(crate) forecast_risks: Vec<String>,
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
    pub(crate) adapter_registry: AdapterRegistry,
    pub(crate) adapter: Option<Arc<dyn LlmAdapter>>,
    pub(crate) latest_telemetry: Option<TelemetrySample>,
    pub(crate) live_context: LiveContext,
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
    pub(crate) fn new(adapter_registry: AdapterRegistry) -> Self {
        Self {
            config: AnomalyRecoveryModeConfig::default(),
            adapter_registry,
            adapter: None,
            latest_telemetry: None,
            live_context: LiveContext::default(),
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
