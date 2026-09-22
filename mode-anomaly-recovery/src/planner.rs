use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use safe::mode_runtime::ModeOutputTx;
use safe::protocol::{AutonomyModeId, Command, CommandEnvelope, TimedCommand};
use safe_llm_adapter::{
    CompletionFinishReason, LlmAdapter, ToolChatCompletion, ToolChatMessage, ToolChatRequest,
};
use safe_sim::{CancellationToken, EdsPatch, SedaroSimulator, SimulationResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::config::{
    AllowedAction, AnomalyRecoveryModeConfig, MetricAggregation, SimulationScenario,
};
use crate::evidence::{EvidenceLedger, telemetry_summary};
use crate::simulation::{
    ScenarioRun, ScenarioRunRequest, ScenarioRunner, UnitMetric, run_and_validate,
};
use crate::types::{
    AnomalyCandidate, AssessmentOutcome, LiveContext, RecoveryDisposition, TelemetrySample,
    ThermalAssessment,
};

const MAX_TURNS: u8 = 6;
const MAX_TOOL_CONTENT_CHARS: usize = 2_000;
const MAX_ASSESSMENT_RATIONALE_CHARS: usize = 400;
const MAX_ASSESSMENT_UNCERTAINTY_CHARS: usize = 160;
const MAX_FORECAST_RISKS: usize = 2;
const MAX_FORECAST_RISK_CHARS: usize = 100;
const MAX_SELECTION_REASON_CHARS: usize = 200;
const CONTEXT_TOOL_OUTPUT_TOKENS: u32 = 128;

#[derive(Clone)]
pub(crate) struct PlanningRequest {
    pub(crate) config: AnomalyRecoveryModeConfig,
    pub(crate) candidates: Vec<AnomalyCandidate>,
    pub(crate) telemetry: TelemetrySample,
    pub(crate) live_context: LiveContext,
    pub(crate) mode_id: AutonomyModeId,
    pub(crate) generation: u64,
    pub(crate) generations: Arc<AtomicU64>,
    pub(crate) active: Arc<AtomicBool>,
    pub(crate) cancel: CancellationToken,
    pub(crate) output: ModeOutputTx,
    pub(crate) adapter: Arc<dyn LlmAdapter>,
    pub(crate) telemetry_version: u64,
    pub(crate) board_version: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArguments {
    scenario_id: String,
    #[serde(default)]
    parameters: HashMap<String, f64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectArguments {
    assessment_id: String,
    anomaly_id: String,
    action_id: String,
    reason: String,
    #[serde(default)]
    evidence_paths: Option<Vec<String>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteAssessmentArguments {
    outcome: AssessmentOutcome,
    disposition: RecoveryDisposition,
    candidate_ids: Vec<String>,
    evidence_ids: Vec<String>,
    rationale: String,
    uncertainty: String,
    #[serde(default)]
    forecast_risks: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadContextArguments {}
#[derive(Serialize)]
struct ToolResult {
    status: &'static str,
    scenario_id: String,
    run_id: u8,
    metrics: HashMap<String, f64>,
    error: Option<String>,
}

struct SedaroScenarioRunner {
    config: AnomalyRecoveryModeConfig,
    telemetry: TelemetrySample,
}

#[async_trait::async_trait]
impl ScenarioRunner for SedaroScenarioRunner {
    async fn run(&self, request: ScenarioRunRequest) -> Result<ScenarioRun> {
        let simulation = self
            .config
            .simulation
            .as_ref()
            .ok_or_else(|| anyhow!("simulation is not configured"))?;
        let scenario = simulation
            .scenarios
            .iter()
            .find(|scenario| scenario.id == request.scenario_id)
            .ok_or_else(|| anyhow!("scenario is not configured"))?;
        let metrics =
            execute_scenario(&self.config, scenario, &self.telemetry, &HashMap::new(), 0).await?;
        let metrics = metrics
            .into_iter()
            .map(|(id, value)| {
                let units = scenario
                    .metrics
                    .iter()
                    .find(|metric| metric.id == id)
                    .map(|metric| metric.units.clone())
                    .unwrap_or_default();
                (id, UnitMetric { value, units })
            })
            .collect();
        Ok(ScenarioRun {
            scenario_id: scenario.id.clone(),
            success: true,
            timed_out: false,
            evidence_revision: request.evidence_revision,
            horizon_days: request.horizon_days,
            metrics,
        })
    }
}

pub(crate) async fn run(request: PlanningRequest) -> Result<()> {
    let started = Instant::now();
    let planning_limit = Duration::from_millis(
        request
            .config
            .llm
            .request_timeout_ms
            .saturating_mul(MAX_TURNS as u64),
    );
    let scenarios = applicable_scenarios(&request.config, &request.candidates);
    let mut ledger = initial_evidence(&request);
    let mut messages = fresh_selection_messages(context_prompt(&request, &ledger)?);
    let mut runs = 0u8;
    let mut assessment: Option<ThermalAssessment> = None;
    for turn in 1..=MAX_TURNS {
        if cancelled(&request) {
            return Ok(());
        }
        if started.elapsed() >= planning_limit {
            bail!("planning time budget exhausted");
        }
        let phase_tools = phase_tools(
            &request.candidates,
            &scenarios,
            runs,
            &ledger,
            assessment.as_ref(),
        );
        let available_tools = tool_names(&phase_tools);
        let max_output_tokens = output_budget(&phase_tools, request.config.llm.max_output_tokens);
        let response = tokio::select! {
            _ = request.cancel.cancelled() => return Ok(()),
            result = chat(&request, messages.clone(), phase_tools) => result?,
        };
        if response.finish_reason == CompletionFinishReason::Length {
            let detail = truncation_detail(
                turn,
                &available_tools,
                request.adapter.kind(),
                &request.config.llm.model,
                max_output_tokens,
            );
            let available_tools = available_tools.join(",");
            if request.config.decision_trace {
                warn!(
                    turn,
                    assistant_content = %sanitize(&response.message.content),
                    parsed_tool_call_count = response.message.tool_calls.len(),
                    "anomaly recovery truncated tool-call diagnostic"
                );
            }
            warn!(
                turn,
                max_turns = MAX_TURNS,
                available_tools,
                adapter = request.adapter.kind(),
                model = %request.config.llm.model,
                max_output_tokens,
                "anomaly recovery tool-call response stopped at token limit"
            );
            bail!(
                "tool-call response stopped at token limit; {detail}; use a tool-capable model with sufficient context"
            );
        }
        let response_chars = response.message.content.chars().count()
            + response
                .message
                .tool_calls
                .iter()
                .map(|call| call.name.chars().count() + call.arguments.to_string().chars().count())
                .sum::<usize>();
        if response_chars > request.config.max_response_chars.saturating_mul(8) {
            bail!("tool-call response exceeded bounded limit");
        }
        let calls = response.message.tool_calls.clone();
        if request.config.decision_trace {
            info!(
                decision_trace = true,
                stage = "tool_call_message",
                turn,
                finish_reason = ?response.finish_reason,
                assistant_content = %sanitize(&response.message.content),
                "anomaly recovery parsed tool-call assistant message"
            );
        }
        info!(
            decision_trace = request.config.decision_trace,
            stage = "tool_calls",
            turn,
            tool_call_count = calls.len(),
            "anomaly recovery received native tool-call response"
        );
        if calls.len() > 1
            && calls.iter().all(|call| {
                matches!(
                    call.name.as_str(),
                    "get_latest_telemetry" | "get_command_board_state"
                )
            })
        {
            for call in &calls {
                match call.name.as_str() {
                    "get_latest_telemetry" => {
                        parse_read_context_arguments(call, "get_latest_telemetry")?;
                        let snapshot = request.live_context.snapshot();
                        let _result = latest_telemetry_result(&request.live_context)?;
                        let status = if snapshot.telemetry.is_some() {
                            "ok"
                        } else {
                            "unavailable"
                        };
                        ledger.record(
                            "telemetry",
                            snapshot.telemetry_version,
                            status,
                            telemetry_summary(&snapshot),
                        );
                    }
                    "get_command_board_state" => {
                        parse_read_context_arguments(call, "get_command_board_state")?;
                        let snapshot = request.live_context.snapshot();
                        let _result = command_board_result(&request.live_context)?;
                        let status = if snapshot.board.is_some() {
                            "ok"
                        } else {
                            "unavailable"
                        };
                        ledger.record(
                            "board",
                            snapshot.board_version,
                            status,
                            board_summary(&snapshot),
                        );
                    }
                    _ => unreachable!("parallel context-call allow-list checked above"),
                }
            }
            messages = fresh_selection_messages(context_prompt(&request, &ledger)?);
            continue;
        }
        if calls.len() != 1 {
            // Do not reinterpret content as a tool request. Give tool-capable
            // models one bounded repair opportunity per remaining turn.
            // A malformed call is not executed, so do not replay its native
            // tool-call envelope without the tool response OpenAI requires.
            messages.push(ToolChatMessage {
                role: response.message.role,
                content: response.message.content,
                tool_calls: Vec::new(),
            });
            messages.push(ToolChatMessage {
                role: "user".into(),
                content: "Use the only available tool now to complete the requested task with the supplied values.".into(),
                tool_calls: Vec::new(),
            });
            continue;
        }
        let call = &calls[0];
        match call.name.as_str() {
            "get_latest_telemetry" => {
                parse_read_context_arguments(call, "get_latest_telemetry")?;
                let snapshot = request.live_context.snapshot();
                let _result = latest_telemetry_result(&request.live_context)?;
                let status = if snapshot.telemetry.is_some() {
                    "ok"
                } else {
                    "unavailable"
                };
                ledger.record(
                    "telemetry",
                    snapshot.telemetry_version,
                    status,
                    telemetry_summary(&snapshot),
                );
                messages = fresh_selection_messages(context_prompt(&request, &ledger)?);
            }
            "get_command_board_state" => {
                parse_read_context_arguments(call, "get_command_board_state")?;
                let snapshot = request.live_context.snapshot();
                let _result = command_board_result(&request.live_context)?;
                let status = if snapshot.board.is_some() {
                    "ok"
                } else {
                    "unavailable"
                };
                ledger.record(
                    "board",
                    snapshot.board_version,
                    status,
                    board_summary(&snapshot),
                );
                messages = fresh_selection_messages(context_prompt(&request, &ledger)?);
            }
            "run_eds_simulation" => {
                if runs >= request.config.simulation.as_ref().map_or(0, |s| s.max_runs) {
                    bail!("simulation run budget exhausted");
                }
                let args: RunArguments = serde_json::from_value(call.arguments.clone())
                    .map_err(|e| anyhow!("invalid run_eds_simulation arguments: {e}"))?;
                let scenario = scenarios
                    .iter()
                    .find(|s| s.id == args.scenario_id)
                    .ok_or_else(|| anyhow!("scenario is not applicable to frozen candidates"))?;
                runs += 1;
                let started_run = Instant::now();
                let result = tokio::select! {
                    _ = request.cancel.cancelled() => return Ok(()),
                    result = execute_scenario(&request.config, scenario, &request.telemetry, &args.parameters, runs) => result,
                };
                let tool_result = match result {
                    Ok(metrics) => ToolResult {
                        status: "ok",
                        scenario_id: scenario.id.clone(),
                        run_id: runs,
                        metrics,
                        error: None,
                    },
                    Err(error) => ToolResult {
                        status: "error",
                        scenario_id: scenario.id.clone(),
                        run_id: runs,
                        metrics: HashMap::new(),
                        error: Some(sanitize(&error.to_string())),
                    },
                };
                ledger.record(
                    "simulation",
                    runs as u64,
                    tool_result.status,
                    serde_json::to_value(&tool_result)?,
                );
                info!(decision_trace = request.config.decision_trace, stage = "simulation_result", turn, runs, elapsed_ms = started_run.elapsed().as_millis() as u64, scenario = %scenario.id, status = tool_result.status, "anomaly recovery simulation tool completed");
                messages = fresh_selection_messages(context_prompt(&request, &ledger)?);
            }
            "complete_thermal_assessment" => {
                let args: CompleteAssessmentArguments =
                    serde_json::from_value(call.arguments.clone()).map_err(|e| {
                        anyhow!("invalid complete_thermal_assessment arguments: {e}")
                    })?;
                let completed = validate_assessment(&request, &ledger, args)?;
                info!(episode_id = %completed.episode_id, revision = completed.revision, outcome = ?completed.outcome, disposition = ?completed.disposition, evidence_ids = ?completed.evidence_ids, "thermal assessment completed");
                if completed.outcome != AssessmentOutcome::ThermalAnomaly
                    || completed.disposition != RecoveryDisposition::EvaluateRecovery
                {
                    return Ok(());
                }
                assessment = Some(completed);
                messages = fresh_selection_messages(context_prompt(&request, &ledger)?);
            }
            "select_recovery_action" => {
                let assessment = assessment.as_ref().ok_or_else(|| {
                    anyhow!("recovery selection requires a completed anomaly assessment")
                })?;
                let args: SelectArguments = match serde_json::from_value(call.arguments.clone()) {
                    Ok(args) => args,
                    Err(error) => {
                        messages = fresh_selection_messages(format!(
                            "{} Selection validation failed: invalid arguments: {error}. Retry select_recovery_action with a non-empty reason and exact IDs.",
                            context_prompt(&request, &ledger)?
                        ));
                        continue;
                    }
                };
                if args.assessment_id != assessment.episode_id {
                    messages = fresh_selection_messages(format!(
                        "{} Selection validation failed: assessment_id must be exactly '{}'. Retry select_recovery_action.",
                        context_prompt(&request, &ledger)?,
                        assessment.episode_id
                    ));
                    continue;
                }
                let (candidate, action) = match evaluate(
                    &request.config,
                    &request.candidates,
                    &args,
                ) {
                    Ok(selection) => selection,
                    Err(error) => {
                        messages = fresh_selection_messages(format!(
                            "{} Selection validation failed: {error}. Retry select_recovery_action with a non-empty reason and exact configured IDs.",
                            context_prompt(&request, &ledger)?
                        ));
                        continue;
                    }
                };
                let simulation = request.config.simulation.as_ref().ok_or_else(|| {
                    anyhow!("recovery requires an action-specific simulation contract")
                })?;
                let recovery = simulation
                    .scenarios
                    .iter()
                    .find(|scenario| {
                        scenario.applicable_rule_ids.contains(&candidate.rule_id)
                            && scenario.modeled_action == Some(action)
                    })
                    .ok_or_else(|| anyhow!("no action-specific recovery scenario is configured"))?;
                let baseline_id = recovery
                    .baseline_scenario_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("recovery scenario has no baseline association"))?;
                let baseline = simulation
                    .scenarios
                    .iter()
                    .find(|scenario| scenario.id == baseline_id)
                    .ok_or_else(|| anyhow!("recovery baseline scenario is not configured"))?;
                let snapshot = request.live_context.snapshot();
                let paired = run_and_validate(
                    Arc::new(SedaroScenarioRunner {
                        config: request.config.clone(),
                        telemetry: request.telemetry.clone(),
                    }),
                    baseline,
                    recovery,
                    action,
                    request.generation,
                    recovery.duration_days,
                )
                .await?;
                if !paired.thermal_benefit_verified {
                    info!(
                        "power-only recovery viability passed; thermal benefit remains unverified"
                    );
                }
                let current = request.live_context.snapshot();
                if snapshot.telemetry_version != current.telemetry_version
                    || snapshot.board_version != current.board_version
                    || request.telemetry_version != current.telemetry_version
                    || request.board_version != current.board_version
                {
                    bail!("simulation result is stale");
                }
                if board_conflicts_or_duplicates(&current, action)? {
                    bail!("recovery action duplicates or conflicts with current command board");
                }
                if cancelled(&request) {
                    return Ok(());
                }
                request
                    .output
                    .command(CommandEnvelope {
                        from: request.mode_id,
                        cmd: TimedCommand::Now(command(action)?),
                    })
                    .await?;
                info!(decision_trace = request.config.decision_trace, stage = "final_validation", turn, runs, anomaly_id = %candidate.anomaly_id, action_id = action.as_str(), elapsed_ms = started.elapsed().as_millis() as u64, "anomaly recovery validated and submitted command board proposal");
                return Ok(());
            }
            _ => bail!("Ollama requested an unknown tool"),
        }
    }
    bail!("planning turn budget exhausted")
}

fn initial_evidence(request: &PlanningRequest) -> EvidenceLedger {
    let snapshot = request.live_context.snapshot();
    let mut ledger = EvidenceLedger::default();
    ledger.record(
        "telemetry",
        snapshot.telemetry_version,
        if snapshot.telemetry.is_some() {
            "ok"
        } else {
            "unavailable"
        },
        telemetry_summary(&snapshot),
    );
    ledger.record(
        "board",
        snapshot.board_version,
        if snapshot.board.is_some() {
            "ok"
        } else {
            "unavailable"
        },
        board_summary(&snapshot),
    );
    ledger
}

fn cancelled(request: &PlanningRequest) -> bool {
    request.cancel.is_cancelled()
        || !request.active.load(Ordering::Acquire)
        || request.generations.load(Ordering::Acquire) != request.generation
}

fn applicable_scenarios<'a>(
    config: &'a AnomalyRecoveryModeConfig,
    candidates: &[AnomalyCandidate],
) -> Vec<&'a SimulationScenario> {
    config
        .simulation
        .as_ref()
        .map(|sim| {
            sim.scenarios
                .iter()
                .filter(|scenario| {
                    candidates.iter().any(|candidate| {
                        scenario.applicable_rule_ids.contains(&candidate.rule_id)
                            && candidate
                                .eligible_actions
                                .iter()
                                .any(|a| scenario.allowed_actions.contains(a))
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn validate_assessment(
    request: &PlanningRequest,
    ledger: &EvidenceLedger,
    args: CompleteAssessmentArguments,
) -> Result<ThermalAssessment> {
    if args.rationale.trim().is_empty()
        || args.rationale.chars().count() > MAX_ASSESSMENT_RATIONALE_CHARS
    {
        bail!("assessment rationale is invalid");
    }
    if args.uncertainty.trim().is_empty()
        || args.uncertainty.chars().count() > MAX_ASSESSMENT_UNCERTAINTY_CHARS
    {
        bail!("assessment uncertainty is invalid");
    }
    if args.forecast_risks.len() > MAX_FORECAST_RISKS
        || args
            .forecast_risks
            .iter()
            .any(|risk| risk.trim().is_empty() || risk.chars().count() > MAX_FORECAST_RISK_CHARS)
    {
        bail!("assessment forecast risks are invalid");
    }
    if !ledger.has_kind("telemetry") || !ledger.has_kind("board") {
        bail!("assessment requires telemetry and command-board tool attempts");
    }
    if args.evidence_ids.is_empty() || !ledger.contains_all(&args.evidence_ids) {
        bail!("assessment cites evidence that is unavailable from this investigation");
    }
    if args.candidate_ids.iter().any(|id| {
        !request
            .candidates
            .iter()
            .any(|candidate| &candidate.anomaly_id == id)
    }) {
        bail!("assessment cites a candidate outside this investigation");
    }
    match (args.outcome, args.disposition) {
        (AssessmentOutcome::ThermalAnomaly, _) => {}
        (_, RecoveryDisposition::EvaluateRecovery) => {
            bail!("only a thermal anomaly may request recovery evaluation")
        }
        (AssessmentOutcome::NoThermalAnomaly, RecoveryDisposition::OperatorReview) => {}
        (AssessmentOutcome::NoThermalAnomaly, RecoveryDisposition::Monitor) => {}
        (
            AssessmentOutcome::Inconclusive,
            RecoveryDisposition::Monitor | RecoveryDisposition::OperatorReview,
        ) => {}
    }
    if args.outcome == AssessmentOutcome::NoThermalAnomaly
        && ledger
            .prompt_value()
            .to_string()
            .contains("\"status\":\"unavailable\"")
    {
        bail!("unavailable required evidence cannot establish no_thermal_anomaly");
    }
    Ok(ThermalAssessment {
        episode_id: format!(
            "{}-{}",
            request
                .candidates
                .first()
                .map_or("thermal", |candidate| candidate.anomaly_id.as_str()),
            request.generation
        ),
        revision: request.generation,
        outcome: args.outcome,
        disposition: args.disposition,
        candidate_ids: args.candidate_ids,
        evidence_ids: args.evidence_ids,
        rationale: args.rationale,
        uncertainty: args.uncertainty,
        forecast_risks: args.forecast_risks,
    })
}

fn context_prompt(request: &PlanningRequest, ledger: &EvidenceLedger) -> Result<String> {
    let text = format!(
        "Build an evidence-backed assessment or select an eligible action when available. Use only supplied values and never invent IDs. Evidence: {} Candidates: {} Actions: {}",
        serde_json::to_string(&ledger.prompt_value())?,
        serde_json::to_string(&compact_candidates(request))?,
        serde_json::to_string(&compact_actions(request))?
    );
    bounded_prompt(request, text)
}

fn fresh_selection_messages(content: String) -> Vec<ToolChatMessage> {
    vec![ToolChatMessage {
        role: "user".into(),
        content,
        tool_calls: Vec::new(),
    }]
}

fn compact_candidates(request: &PlanningRequest) -> Value {
    json!(
        request
            .candidates
            .iter()
            .map(|candidate| json!({
                "id": candidate.anomaly_id,
                "source": candidate.source,
                "observed": candidate.observed,
                "expectation": candidate.expectation,
                "severity": candidate.severity,
                "eligible_actions": candidate.eligible_actions,
            }))
            .collect::<Vec<_>>()
    )
}

fn compact_actions(request: &PlanningRequest) -> Value {
    json!(
        request
            .config
            .action_catalog
            .iter()
            .map(|action| json!({
                "id": action.id,
                "description": action.description,
            }))
            .collect::<Vec<_>>()
    )
}

fn bounded_prompt(request: &PlanningRequest, text: String) -> Result<String> {
    if text.chars().count() > request.config.max_prompt_chars {
        bail!("tool prompt exceeds max_prompt_chars");
    }
    Ok(text)
}

fn simulation_tool(scenarios: &[&SimulationScenario]) -> Value {
    let scenario_ids = scenarios
        .iter()
        .map(|scenario| &scenario.id)
        .collect::<Vec<_>>();
    let mut parameter_properties = serde_json::Map::new();
    for parameter in scenarios.iter().flat_map(|scenario| &scenario.parameters) {
        parameter_properties.entry(parameter.id.clone()).or_insert_with(|| {
            json!({
                "type": "number",
                "description": format!("Optional value from {} through {}", parameter.min, parameter.max)
            })
        });
    }
    json!({"type":"function","function":{"name":"run_eds_simulation","description":"Run one applicable allow-listed local EDS scenario","parameters":{"type":"object","required":["scenario_id"],"properties":{"scenario_id":{"type":"string","description":"Exact ID of the scenario to run","enum":scenario_ids},"parameters":{"type":"object","description":"Optional named numeric scenario parameters within their described bounds","properties":parameter_properties}}}}})
}

fn phase_tools(
    candidates: &[AnomalyCandidate],
    _scenarios: &[&SimulationScenario],
    _runs: u8,
    ledger: &EvidenceLedger,
    assessment: Option<&ThermalAssessment>,
) -> Vec<Value> {
    let mut tools = Vec::new();
    if !ledger.has_kind("telemetry") {
        tools.push(latest_telemetry_tool());
    }
    if !ledger.has_kind("board") {
        tools.push(command_board_tool());
    }
    if !ledger.has_kind("telemetry") || !ledger.has_kind("board") {
        return tools;
    }
    if assessment.is_some() {
        tools.push(select_tool(
            candidates,
            assessment.map(|assessment| assessment.episode_id.as_str()),
        ));
    } else {
        tools.push(assessment_tool(candidates, ledger));
    }
    tools
}

fn tool_names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn truncation_detail(
    turn: u8,
    available_tools: &[String],
    adapter: &str,
    model: &str,
    max_output_tokens: u32,
) -> String {
    format!(
        "turn={turn}/{MAX_TURNS} available_tools=[{}] adapter={adapter} model={model} max_output_tokens={max_output_tokens}",
        available_tools.join(",")
    )
}

fn assessment_tool(candidates: &[AnomalyCandidate], ledger: &EvidenceLedger) -> Value {
    json!({"type":"function","function":{"name":"complete_thermal_assessment","description":"Complete the evidence-backed thermal assessment; this may finish without a recovery command","parameters":{"type":"object","additionalProperties":false,"required":["outcome","disposition","candidate_ids","evidence_ids","rationale","uncertainty"],"properties":{"outcome":{"type":"string","enum":["thermal_anomaly","no_thermal_anomaly","inconclusive"]},"disposition":{"type":"string","enum":["monitor","operator_review","evaluate_recovery"]},"candidate_ids":{"type":"array","items":{"type":"string","enum":candidates.iter().map(|c| c.anomaly_id.as_str()).collect::<Vec<_>>() }},"evidence_ids":{"type":"array","items":{"type":"string","enum":ledger.ids()}},"rationale":{"type":"string","maxLength":MAX_ASSESSMENT_RATIONALE_CHARS},"uncertainty":{"type":"string","maxLength":MAX_ASSESSMENT_UNCERTAINTY_CHARS},"forecast_risks":{"type":"array","maxItems":MAX_FORECAST_RISKS,"items":{"type":"string","maxLength":MAX_FORECAST_RISK_CHARS}}}}}})
}

fn latest_telemetry_tool() -> Value {
    read_context_tool(
        "get_latest_telemetry",
        "Get the latest telemetry snapshot received from SAFE",
    )
}

fn command_board_tool() -> Value {
    read_context_tool(
        "get_command_board_state",
        "Get the current command board snapshot received from SAFE",
    )
}

fn read_context_tool(name: &str, description: &str) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,"parameters":{"type":"object","additionalProperties":false}}})
}

fn select_tool(candidates: &[AnomalyCandidate], assessment_id: Option<&str>) -> Value {
    let mut anomaly_ids = Vec::new();
    let mut action_ids = Vec::new();
    for candidate in candidates {
        if !anomaly_ids.contains(&candidate.anomaly_id.as_str()) {
            anomaly_ids.push(candidate.anomaly_id.as_str());
        }
        for action in &candidate.eligible_actions {
            let action = action.as_str();
            if !action_ids.contains(&action) {
                action_ids.push(action);
            }
        }
    }
    let assessment_schema = assessment_id.map_or_else(
        || json!({"type": "string"}),
        |id| json!({"type": "string", "enum": [id]}),
    );
    json!({"type":"function","function":{"name":"select_recovery_action","description":"Choose one eligible recovery action only after a thermal anomaly assessment requests recovery evaluation","parameters":{"type":"object","required":["assessment_id","anomaly_id","action_id","reason"],"properties":{"assessment_id":assessment_schema,"anomaly_id":{"type":"string","description":"Exact anomaly_id from the candidate list","enum":anomaly_ids},"action_id":{"type":"string","description":"Exact eligible action ID for the selected anomaly","enum":action_ids},"reason":{"type":"string","description":"Brief rationale for this selection","maxLength":MAX_SELECTION_REASON_CHARS}}}}})
}

async fn chat(
    request: &PlanningRequest,
    messages: Vec<ToolChatMessage>,
    tools: Vec<Value>,
) -> Result<ToolChatCompletion> {
    let max_output_tokens = output_budget(&tools, request.config.llm.max_output_tokens);
    let estimated_input_tokens = estimate_request_tokens(&messages, &tools)?;
    let estimated_total_tokens = estimated_input_tokens
        .saturating_add(max_output_tokens)
        .saturating_add(request.config.llm.context_safety_margin_tokens);
    if estimated_total_tokens > request.config.llm.context_window_tokens {
        bail!(
            "tool-call request exceeds context window: estimated_input_tokens={estimated_input_tokens} max_output_tokens={max_output_tokens} context_safety_margin_tokens={} context_window_tokens={}",
            request.config.llm.context_safety_margin_tokens,
            request.config.llm.context_window_tokens,
        );
    }
    request
        .adapter
        .tool_chat(ToolChatRequest {
            model: request.config.llm.model.clone(),
            messages,
            tools,
            temperature: request.config.llm.response_temperature,
            max_output_tokens,
            timeout: Duration::from_millis(request.config.llm.request_timeout_ms),
        })
        .await
        .map_err(|error| {
            anyhow!(
                "{} tool-call request failed: {error}",
                request.adapter.kind()
            )
        })
}

fn output_budget(tools: &[Value], configured_budget: u32) -> u32 {
    if tools.iter().all(|tool| {
        matches!(
            tool.pointer("/function/name").and_then(Value::as_str),
            Some("get_latest_telemetry" | "get_command_board_state")
        )
    }) {
        configured_budget.min(CONTEXT_TOOL_OUTPUT_TOKENS)
    } else {
        configured_budget
    }
}

fn estimate_request_tokens(messages: &[ToolChatMessage], tools: &[Value]) -> Result<u32> {
    let payload = json!({
        "messages": messages.iter().map(|message| json!({
            "role": message.role,
            "content": message.content,
            "tool_calls": message.tool_calls.iter().map(|call| json!({
                "type": "function",
                "function": {"name": call.name, "arguments": call.arguments},
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "tools": tools,
    });
    let bytes = serde_json::to_vec(&payload)?.len();
    // Three bytes per token deliberately overestimates typical JSON tokenization.
    Ok(u32::try_from(bytes.div_ceil(3)).unwrap_or(u32::MAX))
}

fn parse_read_context_arguments(call: &safe_llm_adapter::ToolCall, name: &str) -> Result<()> {
    serde_json::from_value::<ReadContextArguments>(call.arguments.clone())
        .map(|_| ())
        .map_err(|error| anyhow!("invalid {name} arguments: {error}"))
}

fn latest_telemetry_result(context: &LiveContext) -> Result<String> {
    let snapshot = context.snapshot();
    let value = match snapshot.telemetry {
        Some(telemetry) => json!({
            "status": "ok",
            "version": snapshot.telemetry_version,
            "telemetry": telemetry,
        }),
        None => json!({
            "status": "unavailable",
            "version": snapshot.telemetry_version,
            "error": "SAFE has not provided telemetry to this mode",
        }),
    };
    bounded_context_json(value)
}

fn command_board_result(context: &LiveContext) -> Result<String> {
    let snapshot = context.snapshot();
    let value = match snapshot.board {
        Some(board) => json!({
            "status": "ok",
            "version": snapshot.board_version,
            "board": board,
        }),
        None => json!({
            "status": "unavailable",
            "version": snapshot.board_version,
            "error": "SAFE has not provided a command board snapshot to this mode",
        }),
    };
    bounded_context_json(value)
}

fn board_summary(snapshot: &crate::types::LiveContextSnapshot) -> Value {
    json!({
        "available": snapshot.board.is_some(),
        "version": snapshot.board_version,
    })
}

fn bounded_context_json(value: Value) -> Result<String> {
    let text = serde_json::to_string(&value)?;
    if text.chars().count() <= MAX_TOOL_CONTENT_CHARS {
        return Ok(text);
    }
    Ok(serde_json::to_string(&json!({
        "status": "error",
        "error": "context result exceeds tool content limit",
        "limit_chars": MAX_TOOL_CONTENT_CHARS,
    }))?)
}

async fn execute_scenario(
    config: &AnomalyRecoveryModeConfig,
    scenario: &SimulationScenario,
    telemetry: &TelemetrySample,
    parameters: &HashMap<String, f64>,
    _run_id: u8,
) -> Result<HashMap<String, f64>> {
    let patches = build_patches(scenario, telemetry, parameters)?;
    let sim = config
        .simulation
        .as_ref()
        .ok_or_else(|| anyhow!("simulation is not configured"))?;
    let simulator = SedaroSimulator::new(&sim.eds_path)
        .timeout(Duration::from_millis(sim.run_timeout_ms))
        .patch_multi(patches);
    let result = simulator
        .run_collect(scenario.duration_days)
        .await
        .map_err(|e| anyhow!("local EDS run failed: {}", sanitize(&e.to_string())))?;
    if config.decision_trace {
        log_simulation_outputs(scenario, &result);
    }
    extract_metrics(scenario, &result)
}

fn log_simulation_outputs(scenario: &SimulationScenario, result: &SimulationResult) {
    let mut files = result
        .frames_by_file
        .iter()
        .map(|(name, frames)| {
            let fields = frames
                .first()
                .map(|frame| frame.field_names().into_iter().take(32).collect::<Vec<_>>())
                .unwrap_or_default();
            (name.as_str(), frames.len(), fields)
        })
        .collect::<Vec<_>>();
    files.sort_unstable_by_key(|(name, _, _)| *name);
    files.truncate(16);
    info!(
        decision_trace = true,
        stage = "simulation_outputs",
        success = result.success,
        exit_code = ?result.exit_code,
        total_frames = result.total_frames(),
        files = ?files,
        "anomaly recovery collected EDS simulation outputs"
    );

    for metric in &scenario.metrics {
        let matched = result
            .frames_by_file
            .get_key_value(&metric.target_file)
            .or_else(|| {
                result
                    .frames_by_file
                    .iter()
                    .find(|(name, _)| name.ends_with(&metric.target_file))
            });
        let (matched_file, frame_count, fields) = matched.map_or_else(
            || (None, 0, Vec::new()),
            |(name, frames)| {
                let fields = frames
                    .first()
                    .map(|frame| frame.field_names().into_iter().take(32).collect::<Vec<_>>())
                    .unwrap_or_default();
                (Some(name.as_str()), frames.len(), fields)
            },
        );
        info!(
            decision_trace = true,
            stage = "simulation_metric_target",
            metric_id = %metric.id,
            configured_file = %metric.target_file,
            configured_field = %metric.field,
            matched_file = ?matched_file,
            frame_count,
            available_fields = ?fields,
            "anomaly recovery matched configured metric against EDS outputs"
        );
    }
}

pub(crate) fn build_patches(
    scenario: &SimulationScenario,
    telemetry: &TelemetrySample,
    parameters: &HashMap<String, f64>,
) -> Result<Vec<EdsPatch>> {
    let expected = scenario
        .parameters
        .iter()
        .map(|p| p.id.as_str())
        .collect::<HashSet<_>>();
    if parameters
        .keys()
        .any(|key| !expected.contains(key.as_str()))
    {
        bail!("unknown simulation parameter");
    }
    let mut values = scenario
        .patches
        .iter()
        .map(|binding| {
            binding
                .value
                .ok_or_else(|| anyhow!("missing constant patch value"))
                .or_else(|_| {
                    binding
                        .telemetry_path
                        .as_ref()
                        .and_then(|path| {
                            crate::runtime::value_at_payload_path(&telemetry.payload, path)
                        })
                        .and_then(Value::as_f64)
                        .filter(|v| v.is_finite())
                        .ok_or_else(|| anyhow!("telemetry patch value is missing or non-finite"))
                })
        })
        .collect::<Result<Vec<_>>>()?;
    for parameter in &scenario.parameters {
        if let Some(value) = parameters.get(&parameter.id) {
            if !value.is_finite() || *value < parameter.min || *value > parameter.max {
                bail!(
                    "simulation parameter '{}' is outside configured bounds",
                    parameter.id
                );
            }
            values[parameter.patch_index] = *value;
        }
    }
    Ok(scenario
        .patches
        .iter()
        .zip(values)
        .map(|(binding, value)| {
            EdsPatch::new(
                &binding.agent_id,
                &binding.engine,
                &binding.field,
                &binding.type_,
                &format!("{value:?}"),
            )
        })
        .collect())
}

pub(crate) fn extract_metrics(
    scenario: &SimulationScenario,
    result: &SimulationResult,
) -> Result<HashMap<String, f64>> {
    if !result.success {
        bail!("EDS exited unsuccessfully");
    }
    let mut metrics = HashMap::new();
    for metric in &scenario.metrics {
        let values = result.numeric_field_values(&metric.target_file, &metric.field);
        if values.is_empty() || values.iter().any(|v| !v.is_finite()) {
            bail!("configured metric '{}' unavailable", metric.id);
        }
        let value = match metric.aggregation {
            MetricAggregation::Last => *values.last().unwrap(),
            MetricAggregation::Min => values.into_iter().reduce(f64::min).unwrap(),
            MetricAggregation::Max => values.into_iter().reduce(f64::max).unwrap(),
            MetricAggregation::Mean => values.iter().sum::<f64>() / values.len() as f64,
        };
        metrics.insert(metric.id.clone(), value);
    }
    Ok(metrics)
}

fn evaluate<'a>(
    config: &AnomalyRecoveryModeConfig,
    candidates: &'a [AnomalyCandidate],
    args: &SelectArguments,
) -> Result<(&'a AnomalyCandidate, AllowedAction)> {
    if args.reason.trim().is_empty() || args.reason.chars().count() > MAX_SELECTION_REASON_CHARS {
        bail!("final reason is invalid");
    }
    let candidate = candidates
        .iter()
        .find(|c| c.anomaly_id == args.anomaly_id || c.rule_id == args.anomaly_id)
        .ok_or_else(|| anyhow!("final anomaly is not a frozen candidate"))?;
    if args
        .evidence_paths
        .as_ref()
        .is_some_and(|paths| paths.as_slice() != [candidate.path.as_str()])
    {
        bail!("final evidence path is not exactly candidate evidence");
    }
    let action = candidate
        .eligible_actions
        .iter()
        .copied()
        .find(|a| a.as_str() == args.action_id)
        .ok_or_else(|| anyhow!("final action is not eligible"))?;
    if config.action_definition(action).is_none() {
        bail!("final action is not configured");
    }
    Ok((candidate, action))
}

fn board_conflicts_or_duplicates(
    snapshot: &crate::types::LiveContextSnapshot,
    action: AllowedAction,
) -> Result<bool> {
    let Some(board) = snapshot.board.as_ref() else {
        return Ok(false);
    };
    let wanted = serde_json::to_value(&TimedCommand::Now(command(action)?))?;
    let matches = board.proposals.values().any(|(_, candidate, _)| {
        let value = serde_json::to_value(candidate).ok();
        value.as_ref().is_some_and(|value| {
            value == &wanted || (is_recovery_command(value) && is_recovery_command(&wanted))
        })
    });
    Ok(matches)
}

fn is_recovery_command(value: &Value) -> bool {
    value.get("Now").is_some_and(|command| match command {
        Value::String(name) => {
            ["PointSunYaw", "PointNadir", "ThrusterOff"].contains(&name.as_str())
        }
        Value::Object(command) => ["PointSunYaw", "PointNadir", "ThrusterOff"]
            .iter()
            .any(|name| command.contains_key(*name)),
        _ => false,
    })
}
fn command(action: AllowedAction) -> Result<Command> {
    match action {
        AllowedAction::PointNadir => Ok(Command::PointNadir),
        AllowedAction::PointSunYaw => Ok(Command::PointSunYaw),
        AllowedAction::ThrusterOff => Ok(Command::ThrusterOff),
        _ => bail!("unsupported recovery action"),
    }
}
fn bounded_json(result: &ToolResult) -> Result<String> {
    let text = serde_json::to_string(result)?;
    if text.chars().count() > MAX_TOOL_CONTENT_CHARS {
        bail!("bounded simulation result exceeded tool content limit");
    }
    Ok(text)
}
fn sanitize(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(240).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario() -> SimulationScenario {
        serde_json::from_value(json!({
            "id":"thermal", "description":"thermal", "applicable_rule_ids":["r"], "allowed_actions":["point_nadir"], "duration_days":0.1,
            "patches":[{"agent_id":"agent","engine":"power","field":"temperature","type":"f64","telemetry_path":"telemetry.temperature"},{"agent_id":"agent","engine":"power","field":"gain","type":"f64","value":1.0}],
            "parameters":[{"id":"gain","patch_index":1,"min":0.5,"max":2.0}],
            "metrics":[{"id":"peak","quantity":"temperature","units":"C","target_file":"agent.power.jsonl","field":"temperature","aggregation":"max"}]
        })).unwrap()
    }

    fn telemetry() -> TelemetrySample {
        TelemetrySample {
            source: Some("test".into()),
            ts_mono: 1,
            payload: json!({"telemetry":{"temperature":42.0}}),
        }
    }

    fn candidate() -> AnomalyCandidate {
        AnomalyCandidate {
            profile_id: "profile".into(),
            rule_id: "r".into(),
            anomaly_id: "profile-r".into(),
            source: "test".into(),
            ts_mono: 1,
            path: "telemetry.temperature".into(),
            observed: json!(42.0),
            expectation: "at most 30".into(),
            severity: crate::config::AnomalySeverity::High,
            eligible_actions: vec![AllowedAction::PointNadir],
        }
    }

    #[test]
    fn trusted_patches_use_only_config_and_frozen_telemetry() {
        let patches = build_patches(
            &scenario(),
            &telemetry(),
            &HashMap::from([("gain".to_string(), 1.5)]),
        )
        .unwrap();
        assert_eq!(patches.len(), 2);
        assert_eq!(patches[0].value, "42.0");
        assert_eq!(patches[1].value, "1.5");
        assert!(
            build_patches(
                &scenario(),
                &telemetry(),
                &HashMap::from([("path".to_string(), 1.0)])
            )
            .is_err()
        );
        assert!(
            build_patches(
                &scenario(),
                &telemetry(),
                &HashMap::from([("gain".to_string(), 3.0)])
            )
            .is_err()
        );
    }

    #[test]
    fn tool_schemas_use_named_properties_and_allowed_values() {
        let scenario = scenario();
        let simulation = simulation_tool(&[&scenario]);
        assert_eq!(simulation["function"]["name"], "run_eds_simulation");
        assert_eq!(
            simulation["function"]["parameters"]["properties"]["scenario_id"]["enum"][0],
            "thermal"
        );
        assert_eq!(
            simulation["function"]["parameters"]["properties"]["parameters"]["properties"]["gain"]
                ["type"],
            "number"
        );
        assert!(
            simulation["function"]["parameters"]
                .get("additionalProperties")
                .is_none()
        );

        let selection = select_tool(&[candidate()], None);
        assert_eq!(
            selection["function"]["parameters"]["properties"]["anomaly_id"]["enum"][0],
            "profile-r"
        );
        assert_eq!(
            selection["function"]["parameters"]["properties"]["action_id"]["enum"][0],
            "point_nadir"
        );
        assert_eq!(
            selection["function"]["parameters"]["required"],
            json!(["assessment_id", "anomaly_id", "action_id", "reason"])
        );
        assert!(
            selection["function"]["parameters"]["properties"]
                .get("evidence_paths")
                .is_none()
        );
        assert_eq!(
            selection["function"]["parameters"]["properties"]["reason"]["maxLength"],
            MAX_SELECTION_REASON_CHARS
        );

        let mut ledger = EvidenceLedger::default();
        ledger.record("telemetry", 1, "ok", json!({}));
        ledger.record("board", 1, "ok", json!({}));
        let assessment = assessment_tool(&[candidate()], &ledger);
        let properties = &assessment["function"]["parameters"]["properties"];
        assert_eq!(
            properties["rationale"]["maxLength"],
            MAX_ASSESSMENT_RATIONALE_CHARS
        );
        assert_eq!(
            properties["uncertainty"]["maxLength"],
            MAX_ASSESSMENT_UNCERTAINTY_CHARS
        );
        assert_eq!(properties["forecast_risks"]["maxItems"], MAX_FORECAST_RISKS);
        assert_eq!(
            properties["forecast_risks"]["items"]["maxLength"],
            MAX_FORECAST_RISK_CHARS
        );

        let telemetry = latest_telemetry_tool();
        assert_eq!(telemetry["function"]["name"], "get_latest_telemetry");
        assert_eq!(
            telemetry["function"]["parameters"]["additionalProperties"],
            false
        );
        let board = command_board_tool();
        assert_eq!(board["function"]["name"], "get_command_board_state");
    }

    #[test]
    fn context_tools_return_latest_bounded_snapshots() {
        let context = LiveContext::default();
        let unavailable = latest_telemetry_result(&context).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&unavailable).unwrap()["status"],
            "unavailable"
        );

        context.update_telemetry(telemetry());
        context.update_board(safe::protocol::AutonomyModeBoardState::default());
        let latest =
            serde_json::from_str::<Value>(&latest_telemetry_result(&context).unwrap()).unwrap();
        assert_eq!(latest["status"], "ok");
        assert_eq!(latest["version"], 1);
        assert_eq!(latest["telemetry"]["ts_mono"], 1);

        let board =
            serde_json::from_str::<Value>(&command_board_result(&context).unwrap()).unwrap();
        assert_eq!(board["status"], "ok");
        assert_eq!(board["version"], 1);

        context.update_telemetry(TelemetrySample {
            source: None,
            ts_mono: 2,
            payload: json!({"data": "x".repeat(MAX_TOOL_CONTENT_CHARS)}),
        });
        let oversized =
            serde_json::from_str::<Value>(&latest_telemetry_result(&context).unwrap()).unwrap();
        assert_eq!(oversized["status"], "error");
    }

    #[test]
    fn context_tool_arguments_and_phase_tools_are_constrained() {
        let valid = safe_llm_adapter::ToolCall {
            name: "get_latest_telemetry".into(),
            arguments: json!({}),
        };
        assert!(parse_read_context_arguments(&valid, "get_latest_telemetry").is_ok());
        let invalid = safe_llm_adapter::ToolCall {
            arguments: json!({"extra": true}),
            ..valid
        };
        assert!(parse_read_context_arguments(&invalid, "get_latest_telemetry").is_err());

        let scenario = scenario();
        let mut ledger = EvidenceLedger::default();
        ledger.record("telemetry", 1, "ok", json!({}));
        ledger.record("board", 1, "ok", json!({}));
        let before_selection = phase_tools(&[candidate()], &[&scenario], 0, &ledger, None);
        assert_eq!(
            tool_names(&before_selection),
            vec!["complete_thermal_assessment"]
        );
        assert!(
            !before_selection
                .iter()
                .any(|tool| tool["function"]["name"] == "run_eds_simulation")
        );
        assert!(
            !before_selection
                .iter()
                .any(|tool| tool["function"]["name"] == "select_recovery_action")
        );
        let after_assessment = phase_tools(&[candidate()], &[&scenario], 1, &ledger, None);
        assert!(
            after_assessment
                .iter()
                .any(|tool| tool["function"]["name"] == "complete_thermal_assessment")
        );
    }

    #[test]
    fn truncation_diagnostics_identify_turn_and_available_tools() {
        assert_eq!(
            truncation_detail(
                2,
                &[
                    "get_latest_telemetry".to_string(),
                    "get_command_board_state".to_string(),
                ],
                "ollama",
                "mistral:7b",
                1024,
            ),
            "turn=2/6 available_tools=[get_latest_telemetry,get_command_board_state] adapter=ollama model=mistral:7b max_output_tokens=1024"
        );
    }

    #[test]
    fn selection_phase_rebuilds_a_bounded_prompt() {
        let messages = fresh_selection_messages("select now".into());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "select now");
        assert!(messages[0].tool_calls.is_empty());
    }

    #[test]
    fn context_tools_use_a_small_output_budget() {
        assert_eq!(
            output_budget(&[latest_telemetry_tool(), command_board_tool()], 256),
            CONTEXT_TOOL_OUTPUT_TOKENS
        );
        assert_eq!(
            output_budget(
                &[assessment_tool(&[candidate()], &EvidenceLedger::default())],
                256
            ),
            256
        );
    }

    #[test]
    fn request_token_estimate_counts_messages_and_tools() {
        let messages = fresh_selection_messages("inspect context".into());
        let without_tools = estimate_request_tokens(&messages, &[]).unwrap();
        let with_tools = estimate_request_tokens(&messages, &[latest_telemetry_tool()]).unwrap();
        assert!(without_tools > 0);
        assert!(with_tools > without_tools);
    }

    #[test]
    fn board_duplicate_or_conflicting_recovery_is_blocked() {
        let mut snapshot = crate::types::LiveContextSnapshot::default();
        let mut board = safe::protocol::AutonomyModeBoardState::default();
        board.proposals.insert(
            safe::protocol::BoardCmdId("existing".into()),
            (
                safe::protocol::AutonomyModeId(uuid::Uuid::nil()),
                TimedCommand::Now(Command::PointNadir),
                1,
            ),
        );
        snapshot.board = Some(board);
        assert!(board_conflicts_or_duplicates(&snapshot, AllowedAction::PointSunYaw).unwrap());
    }
}
