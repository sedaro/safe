//! Frozen-state simulations supplied as advisory evidence only.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use safe::protocol::{AutonomyModeBoardState, TimedCommand};
use serde::Serialize;

use crate::config::{SimulationConfig, SimulationScenarioRole};
use crate::simulation::{ScenarioRun, ScenarioRunRequest, ScenarioRunner, run_and_validate};

#[derive(Debug, Serialize)]
pub(crate) struct SimulationEvidence {
    pub status: String,
    pub detail: String,
    pub attempted_runs: usize,
    pub runs: Vec<RecordedRun>,
    pub comparisons: Vec<Comparison>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RecordedRun {
    pub scenario_id: String,
    pub result: Option<ScenarioRun>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Comparison {
    pub baseline_id: String,
    pub recovery_id: String,
    pub modeled_action: String,
    pub constraints_passed: bool,
    pub error: Option<String>,
}

impl SimulationEvidence {
    pub fn skipped(detail: impl Into<String>) -> Self {
        Self {
            status: "skipped".into(),
            detail: detail.into(),
            attempted_runs: 0,
            runs: vec![],
            comparisons: vec![],
        }
    }
}

/// NOOPs and rejected proposals have no modeled effects. All other outstanding
/// intent must be accounted for; the current standalone EDS scenarios omit it.
pub(crate) fn board_has_effects(board: &AutonomyModeBoardState) -> bool {
    board.proposals.iter().any(|(id, (_, command, _))| {
        !matches!(command, TimedCommand::NOOP)
            && !board
                .rejected
                .get(id)
                .is_some_and(|entries| !entries.is_empty())
    }) || board
        .source_of_truth
        .iter()
        .chain(board.approved.keys())
        .any(|id| !board.proposals.contains_key(id))
}

struct RecordingRunner {
    inner: Arc<dyn ScenarioRunner>,
    timeout: Duration,
    runs: Mutex<Vec<RecordedRun>>,
}

#[async_trait]
impl ScenarioRunner for RecordingRunner {
    async fn run(&self, request: ScenarioRunRequest) -> Result<ScenarioRun> {
        let scenario_id = request.scenario_id.clone();
        let result = match tokio::time::timeout(self.timeout, self.inner.run(request)).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("scenario timed out")),
        };
        self.runs.lock().unwrap().push(RecordedRun {
            scenario_id,
            result: result.as_ref().ok().cloned(),
            error: result
                .as_ref()
                .err()
                .map(|error| format!("{error:#}").chars().take(1000).collect()),
        });
        result
    }
}

pub(crate) async fn collect(
    config: &SimulationConfig,
    runner: Arc<dyn ScenarioRunner>,
    board: Option<&AutonomyModeBoardState>,
    candidate_ids: &[String],
    post_recovery: bool,
    evidence_revision: u64,
) -> SimulationEvidence {
    let Some(board) = board else {
        return SimulationEvidence::skipped("command board unavailable");
    };
    if board_has_effects(board) {
        return SimulationEvidence::skipped(
            "outstanding board commands are not projected into these EDS scenarios",
        );
    }
    let runner = Arc::new(RecordingRunner {
        inner: runner,
        timeout: Duration::from_millis(config.run_timeout_ms),
        runs: Mutex::new(vec![]),
    });
    let mut comparisons = Vec::new();
    for scenario in &config.scenarios {
        let applicable = post_recovery
            || scenario
                .applicable_rule_ids
                .iter()
                .any(|id| candidate_ids.contains(id));
        if !applicable {
            continue;
        }
        match scenario.role {
            Some(SimulationScenarioRole::Observation) => {
                if runner.runs.lock().unwrap().len() + 1 > config.max_runs as usize {
                    break;
                }
                // Observations project the frozen current state; they do not model an action.
                let _ = runner
                    .run(ScenarioRunRequest {
                        scenario_id: scenario.id.clone(),
                        evidence_revision,
                        horizon_days: scenario.duration_days,
                    })
                    .await;
            }
            Some(SimulationScenarioRole::Recovery) => {
                if runner.runs.lock().unwrap().len() + 2 > config.max_runs as usize {
                    break;
                }
                let Some(baseline) = config
                    .scenarios
                    .iter()
                    .find(|s| Some(s.id.as_str()) == scenario.baseline_scenario_id.as_deref())
                else {
                    continue;
                };
                let Some(action) = scenario.modeled_action else {
                    continue;
                };
                let result = run_and_validate(
                    runner.clone(),
                    baseline,
                    scenario,
                    &config.viability,
                    action,
                    evidence_revision,
                    scenario.duration_days,
                )
                .await;
                comparisons.push(Comparison {
                    baseline_id: baseline.id.clone(),
                    recovery_id: scenario.id.clone(),
                    modeled_action: action.as_str().into(),
                    constraints_passed: result.is_ok(),
                    error: result
                        .err()
                        .map(|error| format!("{error:#}").chars().take(1000).collect()),
                });
            }
            _ => {}
        }
    }
    let runs = std::mem::take(&mut *runner.runs.lock().unwrap());
    if runs.is_empty() {
        return SimulationEvidence::skipped(
            "no applicable simulation scenario within the configured run budget",
        );
    }
    SimulationEvidence { status: "evaluated".into(),
        detail: "Frozen current-state projections and action counterfactuals are advisory evidence only. Observation scenarios do not model a shutdown or any other action. Power-only results do not predict temperature; a thermal model flag alone does not prove benefit. Unconfigured loads and per-packet time alignment remain model assumptions.".into(),
        attempted_runs: runs.len(), runs, comparisons }
}
