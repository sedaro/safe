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
    CompletionFinishReason, CompletionRequest, LlmAdapter, ToolCall, ToolChatCompletion,
    ToolChatMessage, ToolChatRequest,
};
use safe_sim::{CancellationToken, EdsPatch, SedaroSimulator, SimulationResult};
use serde::Deserialize;
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
    let planning_limit = Duration::from_millis(request.config.planner.total_timeout_ms);
    let mut ledger = initial_evidence(&request);
    let mut messages = fresh_selection_messages(context_prompt(&request, &ledger)?);
    let mut repairs = 0u8;
    let mut assessment: Option<ThermalAssessment> = None;
    for turn in 1..=request.config.planner.max_turns {
        if cancelled(&request) {
            return Ok(());
        }
        if started.elapsed() >= planning_limit {
            bail!("planning time budget exhausted");
        }
        let phase_tools = phase_tools(
            &request.candidates,
            &ledger,
            assessment.as_ref(),
            &request.config,
        );
        let available_tools = tool_names(&phase_tools);
        let max_output_tokens = output_budget(&request.config, &phase_tools);
        let response = tokio::select! {
            _ = request.cancel.cancelled() => return Ok(()),
            result = chat(&request, messages.clone(), phase_tools) => result?,
        };
        if response.finish_reason == CompletionFinishReason::Length {
            let detail = truncation_detail(
                request.config.planner.max_turns,
                turn,
                &available_tools,
                request.adapter.kind(),
                &request.config.llm.model,
                max_output_tokens,
            );
            let available_tools = available_tools.join(",");
            if request.config.observability.decision_trace {
                warn!(
                    turn,
                    assistant_content = %sanitize(&response.message.content, request.config.observability.diagnostic_max_chars),
                    parsed_tool_call_count = response.message.tool_calls.len(),
                    provider_attempts = ?response.diagnostic,
                    "anomaly recovery truncated planner diagnostic"
                );
            }
            warn!(
                turn,
                max_turns = request.config.planner.max_turns,
                available_tools,
                adapter = request.adapter.kind(),
                model = %request.config.llm.model,
                max_output_tokens,
                "anomaly recovery planner response stopped at token limit"
            );
            bail!(
                "planner response stopped at token limit; {detail}; use a compatible model with sufficient context"
            );
        }
        validate_response_size(&request.config, &response.message)?;
        let calls = response.message.tool_calls.clone();
        if request.config.observability.decision_trace {
            info!(
                decision_trace = true,
                stage = "tool_call_message",
                turn,
                finish_reason = ?response.finish_reason,
                assistant_content = %sanitize(&response.message.content, request.config.observability.diagnostic_max_chars),
                "anomaly recovery parsed tool-call assistant message"
            );
        }
        info!(
            decision_trace = request.config.observability.decision_trace,
            stage = "tool_calls",
            turn,
            tool_call_count = calls.len(),
            transport = if request.config.llm.enable_tool_calls {
                "native_tools"
            } else {
                "textual_json"
            },
            "anomaly recovery received structured planner response"
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
                        let _result = latest_telemetry_result(
                            &request.live_context,
                            request.config.planner.limits.tool_result_max_chars,
                        )?;
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
                        let _result = command_board_result(
                            &request.live_context,
                            request.config.planner.limits.tool_result_max_chars,
                        )?;
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
        if calls.is_empty() {
            let detail = truncation_detail(
                request.config.planner.max_turns,
                turn,
                &available_tools,
                request.adapter.kind(),
                &request.config.llm.model,
                max_output_tokens,
            );
            if request.config.llm.enable_tool_calls {
                bail!(
                    "native tool call missing; {detail}; server accepted the request but did not produce message.tool_calls; configure a compatible chat template and tool-call parser"
                );
            }
            bail!("textual JSON operation missing; {detail}");
        }
        if calls.len() != 1 {
            repairs = repairs.saturating_add(1);
            if repairs > request.config.planner.repair_attempts {
                bail!("planner repair budget exhausted");
            }
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
                content: request.config.prompts.multiple_calls_repair.clone(),
                tool_calls: Vec::new(),
            });
            continue;
        }
        let call = &calls[0];
        match call.name.as_str() {
            "get_latest_telemetry" => {
                parse_read_context_arguments(call, "get_latest_telemetry")?;
                let snapshot = request.live_context.snapshot();
                let _result = latest_telemetry_result(
                    &request.live_context,
                    request.config.planner.limits.tool_result_max_chars,
                )?;
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
                let _result = command_board_result(
                    &request.live_context,
                    request.config.planner.limits.tool_result_max_chars,
                )?;
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
                        repairs = repairs.saturating_add(1);
                        if repairs > request.config.planner.repair_attempts {
                            bail!("planner repair budget exhausted");
                        }
                        messages = fresh_selection_messages(format!(
                            "{} Selection validation failed: invalid arguments: {error}. {}",
                            context_prompt(&request, &ledger)?,
                            request.config.prompts.selection_repair
                        ));
                        continue;
                    }
                };
                if args.assessment_id != assessment.episode_id {
                    repairs = repairs.saturating_add(1);
                    if repairs > request.config.planner.repair_attempts {
                        bail!("planner repair budget exhausted");
                    }
                    messages = fresh_selection_messages(format!(
                        "{} Selection validation failed: assessment_id must be exactly '{}'. {}",
                        context_prompt(&request, &ledger)?,
                        assessment.episode_id,
                        request.config.prompts.selection_repair
                    ));
                    continue;
                }
                let (candidate, action) =
                    match evaluate(&request.config, &request.candidates, &args) {
                        Ok(selection) => selection,
                        Err(error) => {
                            repairs = repairs.saturating_add(1);
                            if repairs > request.config.planner.repair_attempts {
                                bail!("planner repair budget exhausted");
                            }
                            messages = fresh_selection_messages(format!(
                                "{} Selection validation failed: {error}. {}",
                                context_prompt(&request, &ledger)?,
                                request.config.prompts.selection_repair
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
                    &simulation.viability,
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
                info!(decision_trace = request.config.observability.decision_trace, stage = "final_validation", turn, anomaly_id = %candidate.anomaly_id, action_id = action.as_str(), elapsed_ms = started.elapsed().as_millis() as u64, "anomaly recovery validated and submitted command board proposal");
                return Ok(());
            }
            _ => bail!("model requested an unknown operation"),
        }
    }
    bail!("planning turn budget exhausted")
}

fn initial_evidence(request: &PlanningRequest) -> EvidenceLedger {
    let snapshot = request.live_context.snapshot();
    let mut ledger = EvidenceLedger::new(request.config.evidence.max_items);
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

fn validate_assessment(
    request: &PlanningRequest,
    ledger: &EvidenceLedger,
    args: CompleteAssessmentArguments,
) -> Result<ThermalAssessment> {
    if args.rationale.trim().is_empty()
        || args.rationale.chars().count()
            > request.config.planner.limits.assessment_rationale_max_chars
    {
        bail!("assessment rationale is invalid");
    }
    if args.uncertainty.trim().is_empty()
        || args.uncertainty.chars().count()
            > request
                .config
                .planner
                .limits
                .assessment_uncertainty_max_chars
    {
        bail!("assessment uncertainty is invalid");
    }
    if args.forecast_risks.len() > request.config.planner.limits.forecast_risk_max_items
        || args.forecast_risks.iter().any(|risk| {
            risk.trim().is_empty()
                || risk.chars().count() > request.config.planner.limits.forecast_risk_max_chars
        })
    {
        bail!("assessment forecast risks are invalid");
    }
    if (request.config.evidence.require_telemetry && !ledger.has_kind("telemetry"))
        || (request.config.evidence.require_board && !ledger.has_kind("board"))
    {
        bail!("assessment is missing configured required evidence");
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
        && ((request.config.evidence.require_telemetry && ledger.kind_is_unavailable("telemetry"))
            || (request.config.evidence.require_board && ledger.kind_is_unavailable("board")))
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
        "{} {} {} Use only supplied values and never invent IDs. Evidence: {} Candidates: {} Actions: {}",
        request.config.prompts.planner_instructions,
        request.config.prompts.assessment_instructions,
        request.config.prompts.selection_instructions,
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
                "path": candidate.path,
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
                "preconditions": action.preconditions,
            }))
            .collect::<Vec<_>>()
    )
}

fn bounded_prompt(request: &PlanningRequest, text: String) -> Result<String> {
    if text.chars().count() > request.config.planner.limits.max_prompt_chars {
        bail!("tool prompt exceeds max_prompt_chars");
    }
    Ok(text)
}

fn phase_tools(
    candidates: &[AnomalyCandidate],
    ledger: &EvidenceLedger,
    assessment: Option<&ThermalAssessment>,
    config: &AnomalyRecoveryModeConfig,
) -> Vec<Value> {
    let mut tools = Vec::new();
    if !ledger.has_kind("telemetry") {
        tools.push(latest_telemetry_tool(config));
    }
    if !ledger.has_kind("board") {
        tools.push(command_board_tool(config));
    }
    if !ledger.has_kind("telemetry") || !ledger.has_kind("board") {
        return tools;
    }
    if assessment.is_some() {
        tools.push(select_tool(
            config,
            candidates,
            assessment.map(|assessment| assessment.episode_id.as_str()),
        ));
    } else {
        tools.push(assessment_tool(config, candidates, ledger));
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
    max_turns: u8,
    turn: u8,
    available_tools: &[String],
    adapter: &str,
    model: &str,
    max_output_tokens: u32,
) -> String {
    format!(
        "turn={turn}/{max_turns} available_tools=[{}] adapter={adapter} model={model} max_output_tokens={max_output_tokens}",
        available_tools.join(",")
    )
}

fn assessment_tool(
    config: &AnomalyRecoveryModeConfig,
    candidates: &[AnomalyCandidate],
    ledger: &EvidenceLedger,
) -> Value {
    let limits = &config.planner.limits;
    json!({"type":"function","function":{"name":"complete_thermal_assessment","description":config.prompts.assessment_tool_description,"parameters":{"type":"object","additionalProperties":false,"required":["outcome","disposition","candidate_ids","evidence_ids","rationale","uncertainty"],"properties":{"outcome":{"type":"string","enum":["thermal_anomaly","no_thermal_anomaly","inconclusive"]},"disposition":{"type":"string","enum":["monitor","operator_review","evaluate_recovery"]},"candidate_ids":{"type":"array","items":{"type":"string","enum":candidates.iter().map(|c| c.anomaly_id.as_str()).collect::<Vec<_>>() }},"evidence_ids":{"type":"array","items":{"type":"string","enum":ledger.ids()}},"rationale":{"type":"string","maxLength":limits.assessment_rationale_max_chars},"uncertainty":{"type":"string","maxLength":limits.assessment_uncertainty_max_chars},"forecast_risks":{"type":"array","maxItems":limits.forecast_risk_max_items,"items":{"type":"string","maxLength":limits.forecast_risk_max_chars}}}}}})
}

fn latest_telemetry_tool(config: &AnomalyRecoveryModeConfig) -> Value {
    read_context_tool(
        "get_latest_telemetry",
        &config.prompts.telemetry_tool_description,
    )
}

fn command_board_tool(config: &AnomalyRecoveryModeConfig) -> Value {
    read_context_tool(
        "get_command_board_state",
        &config.prompts.board_tool_description,
    )
}

fn read_context_tool(name: &str, description: &str) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,"parameters":{"type":"object","additionalProperties":false}}})
}

fn select_tool(
    config: &AnomalyRecoveryModeConfig,
    candidates: &[AnomalyCandidate],
    assessment_id: Option<&str>,
) -> Value {
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
    json!({"type":"function","function":{"name":"select_recovery_action","description":config.prompts.selection_tool_description,"parameters":{"type":"object","required":["assessment_id","anomaly_id","action_id","reason"],"properties":{"assessment_id":assessment_schema,"anomaly_id":{"type":"string","description":"Exact anomaly_id from the candidate list","enum":anomaly_ids},"action_id":{"type":"string","description":"Exact eligible action ID for the selected anomaly","enum":action_ids},"reason":{"type":"string","description":"Brief rationale for this selection","maxLength":config.planner.limits.selection_reason_max_chars}}}}})
}

async fn chat(
    request: &PlanningRequest,
    messages: Vec<ToolChatMessage>,
    tools: Vec<Value>,
) -> Result<ToolChatCompletion> {
    let max_output_tokens = output_budget(&request.config, &tools);
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
    dispatch_chat(
        &request.config,
        request.adapter.as_ref(),
        messages,
        tools,
        max_output_tokens,
    )
    .await
}

async fn dispatch_chat(
    config: &AnomalyRecoveryModeConfig,
    adapter: &dyn LlmAdapter,
    messages: Vec<ToolChatMessage>,
    tools: Vec<Value>,
    max_output_tokens: u32,
) -> Result<ToolChatCompletion> {
    let mut last_error = None;
    for attempt in 1..=config.planner.provider_attempts {
        match dispatch_chat_once(
            config,
            adapter,
            messages.clone(),
            tools.clone(),
            max_output_tokens,
        )
        .await
        {
            Ok(response) => return Ok(response),
            Err(error) if attempt < config.planner.provider_attempts => {
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(
                    config.planner.provider_retry_backoff_ms,
                ))
                .await;
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("provider attempt budget exhausted")))
}

async fn dispatch_chat_once(
    config: &AnomalyRecoveryModeConfig,
    adapter: &dyn LlmAdapter,
    messages: Vec<ToolChatMessage>,
    tools: Vec<Value>,
    max_output_tokens: u32,
) -> Result<ToolChatCompletion> {
    if config.llm.enable_tool_calls {
        return adapter
            .tool_chat(ToolChatRequest {
                model: config.llm.model.clone(),
                messages,
                tools,
                temperature: config.llm.response_temperature,
                max_output_tokens,
                timeout: Duration::from_millis(config.llm.request_timeout_ms),
            })
            .await
            .map_err(|error| anyhow!("{} tool-call request failed: {error}", adapter.kind()));
    }

    textual_chat(config, adapter, messages, tools, max_output_tokens).await
}

async fn textual_chat(
    config: &AnomalyRecoveryModeConfig,
    adapter: &dyn LlmAdapter,
    messages: Vec<ToolChatMessage>,
    tools: Vec<Value>,
    max_output_tokens: u32,
) -> Result<ToolChatCompletion> {
    let [tool] = tools.as_slice() else {
        bail!("textual JSON mode requires exactly one available operation");
    };
    if messages
        .iter()
        .any(|message| !message.tool_calls.is_empty())
    {
        bail!("textual JSON mode cannot encode native tool-call history");
    }
    let name = tool
        .pointer("/function/name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("textual JSON operation is missing a name"))?;
    let mut response_schema = tool
        .pointer("/function/parameters")
        .cloned()
        .ok_or_else(|| anyhow!("textual JSON operation is missing a parameter schema"))?;
    let output_guide = match name {
        "complete_thermal_assessment" => {
            let properties = response_schema
                .pointer_mut("/properties")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| anyhow!("assessment operation is missing object properties"))?;
            let rationale_limit = config.planner.limits.textual_assessment_rationale_max_chars;
            let uncertainty_limit = config
                .planner
                .limits
                .textual_assessment_uncertainty_max_chars;
            properties["rationale"]["maxLength"] = json!(rationale_limit);
            properties["uncertainty"]["maxLength"] = json!(uncertainty_limit);
            properties.remove("forecast_risks");
            format!(
                "{} Keep rationale at most {rationale_limit} characters and uncertainty at most {uncertainty_limit} characters.",
                config.prompts.textual_assessment_guide
            )
        }
        "select_recovery_action" => {
            let reason = response_schema
                .pointer_mut("/properties/reason/maxLength")
                .ok_or_else(|| anyhow!("selection operation is missing reason constraints"))?;
            let reason_limit = config.planner.limits.textual_selection_reason_max_chars;
            *reason = json!(reason_limit);
            format!(
                "{} Keep reason at most {reason_limit} characters.",
                config.prompts.textual_selection_guide
            )
        }
        _ => config.prompts.textual_generic_guide.clone(),
    };
    let input = messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let schema = serde_json::to_string(&response_schema)?;
    let prompt = format!(
        "Perform operation `{name}`. {} The complete response must fit within {max_output_tokens} tokens. {output_guide}\nArgument JSON Schema: {schema}\nInput: {input}\nJSON:",
        config.prompts.textual_transport_instructions
    );

    let completion = adapter
        .complete_json_object(CompletionRequest {
            prompt,
            response_schema,
            model: config.llm.model.clone(),
            temperature: config.llm.response_temperature,
            max_output_tokens,
            timeout: Duration::from_millis(config.llm.request_timeout_ms),
        })
        .await
        .map_err(|error| anyhow!("{} textual JSON request failed: {error}", adapter.kind()))?;
    let tool_calls = if completion.finish_reason == CompletionFinishReason::Length {
        Vec::new()
    } else {
        vec![ToolCall {
            name: name.to_string(),
            arguments: serde_json::from_str(completion.text.trim())
                .map_err(|error| anyhow!("invalid textual JSON response: {error}"))?,
        }]
    };
    Ok(ToolChatCompletion {
        message: ToolChatMessage {
            role: "assistant".to_string(),
            content: completion.text,
            tool_calls,
        },
        finish_reason: completion.finish_reason,
        diagnostic: None,
    })
}

fn validate_response_size(
    config: &AnomalyRecoveryModeConfig,
    message: &ToolChatMessage,
) -> Result<()> {
    let response_chars = message.content.chars().count()
        + message
            .tool_calls
            .iter()
            .map(|call| call.name.chars().count() + call.arguments.to_string().chars().count())
            .sum::<usize>();
    if response_chars
        > config
            .planner
            .limits
            .max_response_chars
            .saturating_mul(config.planner.limits.response_size_multiplier)
    {
        bail!("planner response exceeded bounded limit");
    }
    Ok(())
}

fn output_budget(config: &AnomalyRecoveryModeConfig, tools: &[Value]) -> u32 {
    if tools.iter().all(|tool| {
        matches!(
            tool.pointer("/function/name").and_then(Value::as_str),
            Some("get_latest_telemetry" | "get_command_board_state")
        )
    }) {
        config
            .llm
            .max_output_tokens
            .min(config.planner.limits.context_tool_output_tokens)
    } else {
        config.llm.max_output_tokens
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

fn latest_telemetry_result(context: &LiveContext, max_chars: usize) -> Result<String> {
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
    bounded_context_json(value, max_chars)
}

fn command_board_result(context: &LiveContext, max_chars: usize) -> Result<String> {
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
    bounded_context_json(value, max_chars)
}

fn board_summary(snapshot: &crate::types::LiveContextSnapshot) -> Value {
    json!({
        "available": snapshot.board.is_some(),
        "version": snapshot.board_version,
    })
}

fn bounded_context_json(value: Value, max_chars: usize) -> Result<String> {
    let text = serde_json::to_string(&value)?;
    if text.chars().count() <= max_chars {
        return Ok(text);
    }
    Ok(serde_json::to_string(&json!({
        "status": "error",
        "error": "context result exceeds tool content limit",
        "limit_chars": max_chars,
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
        .map_err(|e| {
            anyhow!(
                "local EDS run failed: {}",
                sanitize(&e.to_string(), config.observability.diagnostic_max_chars)
            )
        })?;
    if config.observability.decision_trace {
        log_simulation_outputs(config, scenario, &result);
    }
    extract_metrics(scenario, &result)
}

fn log_simulation_outputs(
    config: &AnomalyRecoveryModeConfig,
    scenario: &SimulationScenario,
    result: &SimulationResult,
) {
    let mut files = result
        .frames_by_file
        .iter()
        .map(|(name, frames)| {
            let fields = frames
                .first()
                .map(|frame| {
                    frame
                        .field_names()
                        .into_iter()
                        .take(config.observability.simulation_trace_max_fields)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (name.as_str(), frames.len(), fields)
        })
        .collect::<Vec<_>>();
    files.sort_unstable_by_key(|(name, _, _)| *name);
    files.truncate(config.observability.simulation_trace_max_files);
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
                    .map(|frame| {
                        frame
                            .field_names()
                            .into_iter()
                            .take(config.observability.simulation_trace_max_fields)
                            .collect::<Vec<_>>()
                    })
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
    if args.reason.trim().is_empty()
        || args.reason.chars().count() > config.planner.limits.selection_reason_max_chars
    {
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
fn sanitize(text: &str, max_chars: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use safe_llm_adapter::{AdapterError, Completion};

    struct TextCompletionAdapter {
        requests: Mutex<Vec<CompletionRequest>>,
        completion: Completion,
    }

    struct FlakyTextAdapter {
        outcomes: Mutex<VecDeque<Result<Completion, AdapterError>>>,
    }

    #[async_trait::async_trait]
    impl LlmAdapter for FlakyTextAdapter {
        fn kind(&self) -> &'static str {
            "flaky-text-test"
        }

        async fn complete(&self, _request: CompletionRequest) -> Result<Completion, AdapterError> {
            self.outcomes.lock().unwrap().pop_front().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl LlmAdapter for TextCompletionAdapter {
        fn kind(&self) -> &'static str {
            "text-test"
        }

        async fn complete(&self, request: CompletionRequest) -> Result<Completion, AdapterError> {
            self.requests.lock().unwrap().push(request);
            Ok(self.completion.clone())
        }
    }

    fn config() -> AnomalyRecoveryModeConfig {
        serde_json::from_str(include_str!("../testdata/static_nominal_profile.json")).unwrap()
    }

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
        let mut config = config();
        config.prompts.selection_tool_description = "Mission selection wording".into();
        let selection = select_tool(&config, &[candidate()], None);
        assert_eq!(
            selection["function"]["parameters"]["properties"]["anomaly_id"]["enum"][0],
            "profile-r"
        );
        assert_eq!(
            selection["function"]["parameters"]["properties"]["action_id"]["enum"][0],
            "point_nadir"
        );
        assert_eq!(
            selection["function"]["description"],
            "Mission selection wording"
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
            config.planner.limits.selection_reason_max_chars
        );

        let mut ledger = EvidenceLedger::default();
        ledger.record("telemetry", 1, "ok", json!({}));
        ledger.record("board", 1, "ok", json!({}));
        let assessment = assessment_tool(&config, &[candidate()], &ledger);
        let properties = &assessment["function"]["parameters"]["properties"];
        assert_eq!(
            properties["rationale"]["maxLength"],
            config.planner.limits.assessment_rationale_max_chars
        );
        assert_eq!(
            properties["uncertainty"]["maxLength"],
            config.planner.limits.assessment_uncertainty_max_chars
        );
        assert_eq!(
            properties["forecast_risks"]["maxItems"],
            config.planner.limits.forecast_risk_max_items
        );
        assert_eq!(
            properties["forecast_risks"]["items"]["maxLength"],
            config.planner.limits.forecast_risk_max_chars
        );

        let telemetry = latest_telemetry_tool(&config);
        assert_eq!(telemetry["function"]["name"], "get_latest_telemetry");
        assert_eq!(
            telemetry["function"]["parameters"]["additionalProperties"],
            false
        );
        let board = command_board_tool(&config);
        assert_eq!(board["function"]["name"], "get_command_board_state");
    }

    #[test]
    fn context_tools_return_latest_bounded_snapshots() {
        let context = LiveContext::default();
        let unavailable = latest_telemetry_result(&context, 2_000).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&unavailable).unwrap()["status"],
            "unavailable"
        );

        context.update_telemetry(telemetry(), 8);
        context.update_board(safe::protocol::AutonomyModeBoardState::default());
        let latest =
            serde_json::from_str::<Value>(&latest_telemetry_result(&context, 2_000).unwrap())
                .unwrap();
        assert_eq!(latest["status"], "ok");
        assert_eq!(latest["version"], 1);
        assert_eq!(latest["telemetry"]["ts_mono"], 1);

        let board =
            serde_json::from_str::<Value>(&command_board_result(&context, 2_000).unwrap()).unwrap();
        assert_eq!(board["status"], "ok");
        assert_eq!(board["version"], 1);

        context.update_telemetry(
            TelemetrySample {
                source: None,
                ts_mono: 2,
                payload: json!({"data": "x".repeat(2_000)}),
            },
            8,
        );
        let oversized =
            serde_json::from_str::<Value>(&latest_telemetry_result(&context, 2_000).unwrap())
                .unwrap();
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

        let config = config();
        let mut ledger = EvidenceLedger::default();
        ledger.record("telemetry", 1, "ok", json!({}));
        ledger.record("board", 1, "ok", json!({}));
        let before_selection = phase_tools(&[candidate()], &ledger, None, &config);
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
        let after_assessment = phase_tools(&[candidate()], &ledger, None, &config);
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
                6,
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
        let config = config();
        assert_eq!(
            output_budget(
                &config,
                &[latest_telemetry_tool(&config), command_board_tool(&config)]
            ),
            config.planner.limits.context_tool_output_tokens
        );
        assert_eq!(
            output_budget(
                &config,
                &[assessment_tool(
                    &config,
                    &[candidate()],
                    &EvidenceLedger::default()
                )],
            ),
            256
        );
    }

    #[test]
    fn request_token_estimate_counts_messages_and_tools() {
        let messages = fresh_selection_messages("inspect context".into());
        let config = config();
        let without_tools = estimate_request_tokens(&messages, &[]).unwrap();
        let with_tools =
            estimate_request_tokens(&messages, &[latest_telemetry_tool(&config)]).unwrap();
        assert!(without_tools > 0);
        assert!(with_tools > without_tools);
    }

    #[tokio::test]
    async fn textual_mode_uses_completion_and_normalizes_json_as_the_available_operation() {
        let mut mode_config = config();
        mode_config.llm.enable_tool_calls = false;
        mode_config.prompts.textual_transport_instructions =
            "MISSION JSON TRANSPORT INSTRUCTIONS".into();
        let adapter = TextCompletionAdapter {
            requests: Mutex::new(Vec::new()),
            completion: Completion {
                text: r#"{"outcome":"inconclusive","disposition":"monitor","candidate_ids":[],"evidence_ids":[],"rationale":"More evidence is needed.","uncertainty":"Telemetry is limited."}"#.into(),
                finish_reason: CompletionFinishReason::Complete,
            },
        };
        let mut ledger = EvidenceLedger::default();
        ledger.record("telemetry", 1, "ok", json!({}));
        ledger.record("board", 1, "ok", json!({}));

        let response = dispatch_chat(
            &mode_config,
            &adapter,
            fresh_selection_messages("assess now".into()),
            vec![assessment_tool(&mode_config, &[candidate()], &ledger)],
            256,
        )
        .await
        .unwrap();

        assert_eq!(response.message.tool_calls.len(), 1);
        assert_eq!(
            response.message.tool_calls[0].name,
            "complete_thermal_assessment"
        );
        assert_eq!(
            response.message.tool_calls[0].arguments["outcome"],
            "inconclusive"
        );
        let requests = adapter.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let prompt = &requests[0].prompt;
        assert!(prompt.starts_with("Perform operation `complete_thermal_assessment`"));
        assert!(prompt.contains("MISSION JSON TRANSPORT INSTRUCTIONS"));
        assert!(prompt.contains("complete response must fit within 256 tokens"));
        assert!(prompt.contains(
            "Emit required keys in this exact order: outcome, disposition, candidate_ids, evidence_ids, rationale, uncertainty"
        ));
        assert!(prompt.contains("Argument JSON Schema: {\"additionalProperties\":false"));
        assert!(prompt.contains("\"required\":[\"outcome\",\"disposition\""));
        assert!(!prompt.contains("forecast_risks"));
        assert!(prompt.contains("Input: assess now\nJSON:"));
        assert!(
            prompt.find("Argument JSON Schema:").unwrap() < prompt.find("Input:").unwrap(),
            "the output contract should precede untrusted input"
        );
        assert_eq!(requests[0].response_schema["type"], "object");
        assert_eq!(
            requests[0].response_schema["properties"]["rationale"]["maxLength"],
            mode_config
                .planner
                .limits
                .textual_assessment_rationale_max_chars
        );
        assert_eq!(
            requests[0].response_schema["properties"]["uncertainty"]["maxLength"],
            mode_config
                .planner
                .limits
                .textual_assessment_uncertainty_max_chars
        );
        assert!(
            requests[0].response_schema["properties"]
                .get("forecast_risks")
                .is_none()
        );
    }

    #[tokio::test]
    async fn configured_provider_attempts_retry_transient_failures() {
        let mut mode_config = config();
        mode_config.llm.enable_tool_calls = false;
        mode_config.planner.provider_attempts = 2;
        mode_config.planner.provider_retry_backoff_ms = 0;
        let adapter = FlakyTextAdapter {
            outcomes: Mutex::new(VecDeque::from([
                Err(AdapterError::Timeout),
                Ok(Completion {
                    text: r#"{"outcome":"inconclusive","disposition":"monitor","candidate_ids":[],"evidence_ids":[],"rationale":"Retry succeeded.","uncertainty":"Evidence remains limited."}"#.into(),
                    finish_reason: CompletionFinishReason::Complete,
                }),
            ])),
        };
        let response = dispatch_chat(
            &mode_config,
            &adapter,
            fresh_selection_messages("assess now".into()),
            vec![assessment_tool(
                &mode_config,
                &[candidate()],
                &EvidenceLedger::default(),
            )],
            256,
        )
        .await
        .expect("second configured provider attempt should succeed");
        assert_eq!(response.message.tool_calls.len(), 1);
        assert!(adapter.outcomes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn textual_mode_normalizes_recovery_selection() {
        let mode_config = config();
        let adapter = TextCompletionAdapter {
            requests: Mutex::new(Vec::new()),
            completion: Completion {
                text: r#"{"assessment_id":"assessment-1","anomaly_id":"profile-r","action_id":"point_nadir","reason":"Use the configured recovery."}"#.into(),
                finish_reason: CompletionFinishReason::Complete,
            },
        };
        let response = textual_chat(
            &mode_config,
            &adapter,
            fresh_selection_messages("select now".into()),
            vec![select_tool(
                &mode_config,
                &[candidate()],
                Some("assessment-1"),
            )],
            256,
        )
        .await
        .unwrap();

        assert_eq!(response.message.tool_calls.len(), 1);
        assert_eq!(
            response.message.tool_calls[0].name,
            "select_recovery_action"
        );
        assert_eq!(
            response.message.tool_calls[0].arguments["action_id"],
            "point_nadir"
        );
        let requests = adapter.requests.lock().unwrap();
        assert!(requests[0].prompt.contains(
            "Emit required keys in this exact order: assessment_id, anomaly_id, action_id, reason"
        ));
        assert_eq!(
            requests[0].response_schema["properties"]["reason"]["maxLength"],
            mode_config
                .planner
                .limits
                .textual_selection_reason_max_chars
        );
    }

    #[tokio::test]
    async fn textual_assessment_can_exceed_the_legacy_completion_limit() {
        let mode_config = config();
        let limits = &mode_config.planner.limits;
        let text = json!({
            "outcome": "inconclusive",
            "disposition": "monitor",
            "candidate_ids": ["profile-r"],
            "evidence_ids": ["telemetry:1", "board:1"],
            "rationale": "r".repeat(limits.assessment_rationale_max_chars),
            "uncertainty": "u".repeat(limits.assessment_uncertainty_max_chars),
            "forecast_risks": [
                "a".repeat(limits.forecast_risk_max_chars),
                "b".repeat(limits.forecast_risk_max_chars)
            ]
        })
        .to_string();
        assert!(text.chars().count() > limits.max_response_chars);
        let adapter = TextCompletionAdapter {
            requests: Mutex::new(Vec::new()),
            completion: Completion {
                text,
                finish_reason: CompletionFinishReason::Complete,
            },
        };

        let response = textual_chat(
            &mode_config,
            &adapter,
            fresh_selection_messages("assess now".into()),
            vec![assessment_tool(
                &mode_config,
                &[candidate()],
                &EvidenceLedger::default(),
            )],
            256,
        )
        .await
        .expect("bounded assessment fields may exceed the legacy completion limit");
        validate_response_size(&mode_config, &response.message)
            .expect("assessment should remain within the structured planner bound");
    }

    #[tokio::test]
    async fn textual_mode_rejects_ambiguous_and_malformed_responses() {
        let mut mode_config = config();
        let operation = assessment_tool(&mode_config, &[candidate()], &EvidenceLedger::default());
        let messages = fresh_selection_messages("assess now".into());
        let malformed = TextCompletionAdapter {
            requests: Mutex::new(Vec::new()),
            completion: Completion {
                text: "not JSON".into(),
                finish_reason: CompletionFinishReason::Complete,
            },
        };
        assert!(
            textual_chat(
                &mode_config,
                &malformed,
                messages.clone(),
                vec![operation.clone()],
                256,
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("invalid textual JSON response")
        );

        let valid = TextCompletionAdapter {
            requests: Mutex::new(Vec::new()),
            completion: Completion {
                text: "{}".into(),
                finish_reason: CompletionFinishReason::Complete,
            },
        };
        assert!(
            textual_chat(
                &mode_config,
                &valid,
                messages.clone(),
                vec![operation.clone(), operation.clone()],
                256,
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("exactly one available operation")
        );

        mode_config.planner.limits.max_response_chars = 4;
        let oversized = TextCompletionAdapter {
            requests: Mutex::new(Vec::new()),
            completion: Completion {
                text: format!(r#"{{"x":"{}"}}"#, "x".repeat(40)),
                finish_reason: CompletionFinishReason::Complete,
            },
        };
        let response = textual_chat(&mode_config, &oversized, messages, vec![operation], 256)
            .await
            .expect("textual transport should use the shared planner response bound");
        assert!(
            validate_response_size(&mode_config, &response.message)
                .unwrap_err()
                .to_string()
                .contains("bounded limit")
        );
    }

    #[tokio::test]
    async fn textual_mode_preserves_truncation_without_parsing_partial_json() {
        let mode_config = config();
        let adapter = TextCompletionAdapter {
            requests: Mutex::new(Vec::new()),
            completion: Completion {
                text: "{".into(),
                finish_reason: CompletionFinishReason::Length,
            },
        };
        let response = textual_chat(
            &mode_config,
            &adapter,
            fresh_selection_messages("assess now".into()),
            vec![assessment_tool(
                &mode_config,
                &[candidate()],
                &EvidenceLedger::default(),
            )],
            256,
        )
        .await
        .unwrap();
        assert_eq!(response.finish_reason, CompletionFinishReason::Length);
        assert!(response.message.tool_calls.is_empty());
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
