use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use safe::mode_runtime::ModeOutputTx;
use safe::protocol::{AutonomyModeId, Command, CommandEnvelope, TimedCommand};
use safe_sim::{CancellationToken, EdsPatch, SedaroSimulator, SimulationResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::timeout;
use tracing::info;

use crate::config::{
    AllowedAction, AnomalyRecoveryModeConfig, MetricAggregation, SimulationScenario,
};
use crate::http_client;
use crate::types::{AnomalyCandidate, TelemetrySample};

const MAX_TURNS: u8 = 6;
const MAX_TOOL_CONTENT_CHARS: usize = 2_000;
const SELECT_INSTRUCTION: &str = "The required simulation is complete. Use select_recovery_action to choose the best eligible action for one supplied anomaly candidate based on the simulation result.";

#[derive(Clone)]
pub(crate) struct PlanningRequest {
    pub(crate) config: AnomalyRecoveryModeConfig,
    pub(crate) candidates: Vec<AnomalyCandidate>,
    pub(crate) telemetry: TelemetrySample,
    pub(crate) mode_id: AutonomyModeId,
    pub(crate) generation: u64,
    pub(crate) generations: Arc<AtomicU64>,
    pub(crate) active: Arc<AtomicBool>,
    pub(crate) cancel: CancellationToken,
    pub(crate) output: ModeOutputTx,
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    tools: Vec<Value>,
    stream: bool,
    options: ChatOptions,
}
#[derive(Serialize, Deserialize, Clone)]
struct ChatMessage {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
}
#[derive(Serialize)]
struct ChatOptions {
    temperature: f64,
    num_predict: u32,
}
#[derive(Deserialize)]
struct ChatResponse {
    message: ChatMessage,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    eval_count: Option<u32>,
}
#[derive(Serialize, Deserialize, Clone)]
struct ToolCall {
    function: ToolFunction,
}
#[derive(Serialize, Deserialize, Clone)]
struct ToolFunction {
    name: String,
    arguments: Value,
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
    anomaly_id: String,
    action_id: String,
    reason: String,
    #[serde(default)]
    evidence_paths: Option<Vec<String>>,
}
#[derive(Serialize)]
struct ToolResult {
    status: &'static str,
    scenario_id: String,
    run_id: u8,
    metrics: HashMap<String, f64>,
    error: Option<String>,
}

pub(crate) async fn run(request: PlanningRequest) -> Result<()> {
    let started = Instant::now();
    let planning_limit = Duration::from_millis(
        request
            .config
            .request_timeout_ms
            .saturating_mul(MAX_TURNS as u64),
    );
    let scenarios = applicable_scenarios(&request.config, &request.candidates);
    let mut messages = vec![ChatMessage {
        // Mistral receives concrete tool tasks reliably as a user turn. Tool
        // definitions remain native Ollama fields rather than prompt syntax.
        role: "user".into(),
        content: prompt(&request, &scenarios)?,
        tool_calls: None,
        tool_name: None,
    }];
    let mut runs = 0u8;
    let mut used_scenarios = Vec::new();
    for turn in 1..=MAX_TURNS {
        if cancelled(&request) {
            return Ok(());
        }
        if started.elapsed() >= planning_limit {
            bail!("planning time budget exhausted");
        }
        let phase_tools = if scenarios.is_empty() || runs > 0 {
            vec![select_tool(&request.candidates)]
        } else {
            vec![simulation_tool(&scenarios)]
        };
        let response = tokio::select! {
            _ = request.cancel.cancelled() => return Ok(()),
            result = chat(&request.config, messages.clone(), phase_tools) => result?,
        };
        if response.done_reason.as_deref() == Some("length") {
            bail!(
                "Ollama response stopped at token limit; use a tool-capable model with sufficient context"
            );
        }
        let calls = response.message.tool_calls.clone().unwrap_or_default();
        if request.config.decision_trace {
            info!(
                decision_trace = true,
                stage = "ollama_message",
                turn,
                done = response.done,
                done_reason = ?response.done_reason,
                eval_count = ?response.eval_count,
                assistant_content = %sanitize(&response.message.content),
                "anomaly recovery parsed Ollama assistant message"
            );
        }
        info!(
            decision_trace = request.config.decision_trace,
            stage = "ollama_tool_calls",
            turn,
            tool_call_count = calls.len(),
            "anomaly recovery received Ollama tool-call response"
        );
        if calls.len() != 1 {
            // Do not reinterpret content as a tool request. Give tool-capable
            // models one bounded repair opportunity per remaining turn.
            messages.push(response.message);
            messages.push(ChatMessage {
                role: "user".into(),
                content: "Use the only available tool now to complete the requested task with the supplied values.".into(),
                tool_calls: None,
                tool_name: None,
            });
            continue;
        }
        let call = &calls[0].function;
        match call.name.as_str() {
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
                used_scenarios.push(scenario);
                info!(decision_trace = request.config.decision_trace, stage = "simulation_result", turn, runs, elapsed_ms = started_run.elapsed().as_millis() as u64, scenario = %scenario.id, status = tool_result.status, "anomaly recovery simulation tool completed");
                messages.push(response.message);
                messages.push(ChatMessage {
                    role: "tool".into(),
                    content: bounded_json(&tool_result)?,
                    tool_calls: None,
                    tool_name: Some("run_eds_simulation".into()),
                });
                messages.push(ChatMessage {
                    role: "user".into(),
                    content: SELECT_INSTRUCTION.into(),
                    tool_calls: None,
                    tool_name: None,
                });
            }
            "select_recovery_action" => {
                if !scenarios.is_empty() && runs == 0 {
                    bail!(
                        "an applicable simulation scenario requires a simulation before final action selection"
                    );
                }
                let args: SelectArguments = serde_json::from_value(call.arguments.clone())
                    .map_err(|e| anyhow!("invalid select_recovery_action arguments: {e}"))?;
                let (candidate, action) = evaluate(&request.config, &request.candidates, &args)?;
                if !scenarios.is_empty()
                    && !used_scenarios.iter().any(|scenario| {
                        scenario.applicable_rule_ids.contains(&candidate.rule_id)
                            && scenario.allowed_actions.contains(&action)
                    })
                {
                    bail!("final action is not allowed by a completed scenario");
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

fn prompt(request: &PlanningRequest, scenarios: &[&SimulationScenario]) -> Result<String> {
    let value = json!({"goal": request.config.goal, "instructions": request.config.analysis_instructions, "candidates": request.candidates, "scenarios": scenarios.iter().map(|s| json!({"id":s.id,"description":s.description,"applicable_rule_ids":s.applicable_rule_ids,"allowed_actions":s.allowed_actions,"parameters":s.parameters.iter().map(|p| json!({"id":p.id,"min":p.min,"max":p.max})).collect::<Vec<_>>(),"metrics":s.metrics.iter().map(|m| &m.id).collect::<Vec<_>>() })).collect::<Vec<_>>()});
    let next_step = if scenarios.is_empty() {
        "Choose the best eligible recovery action for one supplied anomaly candidate by using select_recovery_action."
    } else {
        "Start by using run_eds_simulation for one applicable supplied scenario."
    };
    let text = format!(
        "You are a constrained SAFE recovery advisor. {next_step} Use only the provided tool and supplied values. Never invent IDs. Context: {}",
        serde_json::to_string(&value)?
    );
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

fn select_tool(candidates: &[AnomalyCandidate]) -> Value {
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
    json!({"type":"function","function":{"name":"select_recovery_action","description":"Choose one eligible recovery action for one supplied anomaly","parameters":{"type":"object","required":["anomaly_id","action_id","reason"],"properties":{"anomaly_id":{"type":"string","description":"Exact anomaly_id from the candidate list","enum":anomaly_ids},"action_id":{"type":"string","description":"Exact eligible action ID for the selected anomaly","enum":action_ids},"reason":{"type":"string","description":"Brief rationale for this selection"}}}}})
}

async fn chat(
    config: &AnomalyRecoveryModeConfig,
    messages: Vec<ChatMessage>,
    tools: Vec<Value>,
) -> Result<ChatResponse> {
    let body = serde_json::to_string(&ChatRequest {
        model: config.model.clone(),
        messages,
        tools,
        stream: false,
        options: ChatOptions {
            temperature: config.response_temperature,
            num_predict: config.num_predict,
        },
    })?;
    if config.decision_trace {
        info!(
            decision_trace = true,
            stage = "ollama_request_body",
            request_body = %body,
            "anomaly recovery serialized Ollama chat request"
        );
    }
    info!(
        decision_trace = config.decision_trace,
        stage = "ollama_request",
        host = %config.ollama_host,
        port = config.ollama_port,
        path = %config.ollama_path,
        model = %config.model,
        request_bytes = body.len(),
        "anomaly recovery sending Ollama chat request"
    );
    let result = timeout(
        Duration::from_millis(config.request_timeout_ms),
        http_client::post_json(
            &config.ollama_host,
            config.ollama_port,
            &config.ollama_path,
            &body,
        ),
    )
    .await
    .map_err(|_| anyhow!("Ollama chat request timed out; verify local tool-capable model"))??;
    info!(
        decision_trace = config.decision_trace,
        stage = "ollama_response",
        status = result.status,
        response_bytes = result.body.len(),
        "anomaly recovery received Ollama chat response"
    );
    if !(200..300).contains(&result.status) {
        bail!("Ollama HTTP {}: {}", result.status, sanitize(&result.body));
    }
    if result.body.chars().count() > config.max_response_chars.saturating_mul(8) {
        bail!("Ollama chat payload exceeded bounded limit");
    }
    serde_json::from_str(&result.body).map_err(|e| anyhow!("invalid Ollama chat response: {e}"))
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
    extract_metrics(scenario, &result)
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
    if args.reason.trim().is_empty() || args.reason.chars().count() > 400 {
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
            "metrics":[{"id":"peak","target_file":"agent.power.jsonl","field":"temperature","aggregation":"max"}]
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

        let selection = select_tool(&[candidate()]);
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
            json!(["anomaly_id", "action_id", "reason"])
        );
        assert!(
            selection["function"]["parameters"]["properties"]
                .get("evidence_paths")
                .is_none()
        );
    }

    #[test]
    fn tool_result_names_the_executed_tool() {
        let message = ChatMessage {
            role: "tool".into(),
            content: "{}".into(),
            tool_calls: None,
            tool_name: Some("run_eds_simulation".into()),
        };
        let value = serde_json::to_value(message).unwrap();
        assert_eq!(value["tool_name"], "run_eds_simulation");
        assert!(value.get("tool_calls").is_none());
    }

    #[test]
    fn selection_phase_instruction_supersedes_simulation_step() {
        assert!(SELECT_INSTRUCTION.contains("select_recovery_action"));
        assert!(!SELECT_INSTRUCTION.contains("run_eds_simulation"));
    }
}
