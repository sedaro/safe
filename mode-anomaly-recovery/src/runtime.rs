use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::header::CONTENT_TYPE;
use safe::mode_runtime::{ModeHandler, ModeRuntime};
use safe::protocol::{AutonomyModeBoardState, Command, CommandEnvelope, TimedCommand};
use safe::telemetry_frame::TelemetryFrame;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::config::{AllowedAction, LlmAdvisorModeConfig, NominalRule, NominalRuleKind};
use crate::types::{AnomalyCandidate, LlmAdvisorMode, TelemetrySample};

#[derive(Debug, Clone, Serialize)]
struct DecisionEnvelope {
    goal: String,
    analysis_instructions: String,
    board: BoardSummary,
    candidates: Vec<AnomalyCandidate>,
    action_catalog: Vec<ActionPromptEntry>,
}

#[derive(Debug, Clone, Serialize)]
struct BoardSummary {
    proposal_count: usize,
    approved_proposals: usize,
    rejected_proposals: usize,
}

#[derive(Debug, Clone, Serialize)]
struct ActionPromptEntry {
    id: AllowedAction,
    description: String,
    preconditions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OllamaResponse {
    response: String,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    eval_count: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AdvisorDecision {
    anomaly_id: String,
    action_id: String,
    reason: String,
    evidence_paths: Vec<String>,
}

enum RuleEvaluation {
    Normal,
    Violation,
    Invalid(String),
}

impl LlmAdvisorMode {
    fn log_decision_trace(&self, stage: &str, detail: impl std::fmt::Display) {
        if self.config.decision_trace {
            let detail = trace_text(&detail.to_string());
            info!(decision_trace = true, stage, "LLM DEMO | {detail}");
        }
    }

    fn log_planning_error(
        &self,
        hook: &str,
        stage: &str,
        err: &anyhow::Error,
        telemetry: Option<&TelemetrySample>,
    ) {
        let ts_mono = telemetry.map(|t| t.ts_mono);
        let source = telemetry.and_then(|t| t.source.as_deref());
        warn!(
            hook = %hook,
            stage = %stage,
            has_board_snapshot = self.has_board_snapshot,
            source = ?source,
            ts_mono = ?ts_mono,
            reason = %format!("{err:#}"),
            "anomaly recovery planning failed without emitting a command"
        );
    }

    fn build_board_summary(board: &AutonomyModeBoardState) -> BoardSummary {
        BoardSummary {
            proposal_count: board.proposals.len(),
            approved_proposals: board.approved.len(),
            rejected_proposals: board.rejected.len(),
        }
    }

    fn action_prompt_entries(&self, candidates: &[AnomalyCandidate]) -> Vec<ActionPromptEntry> {
        let eligible_actions = candidates
            .iter()
            .flat_map(|candidate| candidate.eligible_actions.iter().copied())
            .collect::<HashSet<_>>();

        self.config
            .action_catalog
            .iter()
            .filter(|action| eligible_actions.contains(&action.id))
            .map(|action| ActionPromptEntry {
                id: action.id,
                description: action.description.clone(),
                preconditions: action.preconditions.clone(),
            })
            .collect()
    }

    fn build_decision_envelope(&self, candidates: Vec<AnomalyCandidate>) -> DecisionEnvelope {
        DecisionEnvelope {
            goal: self.config.goal.clone(),
            analysis_instructions: self.config.analysis_instructions.clone(),
            board: Self::build_board_summary(&self.latest_board_snapshot),
            action_catalog: self.action_prompt_entries(&candidates),
            candidates,
        }
    }

    fn build_prompt(&self, envelope: &DecisionEnvelope, feedback: Option<&str>) -> Result<String> {
        let mut prompt = String::new();
        prompt.push_str("You are a constrained telemetry anomaly action selector.\n");
        prompt.push_str(
            "The supplied candidates are already established by configured nominal-profile rules. Do not reinterpret them as baseline observations.\n",
        );
        prompt.push_str("Choose exactly one candidate and exactly one action that candidate lists in eligible_actions.\n");
        prompt.push_str("Copy the selected candidate's anomaly_id exactly; do not construct a new identifier.\n");
        prompt.push_str("Return strict JSON only with this schema: ");
        prompt.push_str(
            "{\"anomaly_id\":string,\"action_id\":string,\"reason\":string,\"evidence_paths\":array<string>}.\n",
        );
        prompt.push_str(
            "evidence_paths must contain exactly the selected candidate's path. No markdown or text outside JSON.\n",
        );
        if let Some(feedback) = feedback {
            prompt.push_str("Previous attempt failed validation. Repair it using this feedback:\n");
            prompt.push_str(feedback);
            prompt.push('\n');
        }
        prompt.push_str("Decision envelope JSON: ");

        let envelope_json = serde_json::to_string(envelope)?;
        let remaining = self
            .config
            .max_prompt_chars
            .checked_sub(prompt.chars().count())
            .ok_or_else(|| anyhow!("prompt instructions exceed max_prompt_chars"))?;
        if envelope_json.chars().count() > remaining {
            return Err(anyhow!(
                "decision envelope is {} characters but only {} remain in max_prompt_chars",
                envelope_json.chars().count(),
                remaining
            ));
        }
        prompt.push_str(&envelope_json);
        Ok(prompt)
    }

    fn build_repair_feedback(
        &self,
        stage: &str,
        err: &anyhow::Error,
        prior_response: &str,
    ) -> String {
        let feedback = format!(
            "Failure stage: {stage}. Error: {err:#}. Previous response: {}",
            clip_chars(prior_response, self.config.max_feedback_chars)
        );
        clip_chars(&feedback, self.config.max_feedback_chars)
    }

    async fn plan_decision_with_feedback_loop(
        &self,
        envelope: &DecisionEnvelope,
    ) -> Result<AdvisorDecision> {
        let mut feedback: Option<String> = None;
        let max_attempts = self.config.max_decision_attempts as usize;

        for attempt in 1..=max_attempts {
            let prompt = self
                .build_prompt(envelope, feedback.as_deref())
                .map_err(|e| {
                    anyhow!("attempt {attempt}/{max_attempts} build_prompt failed: {e:#}")
                })?;

            self.log_decision_trace(
                "request",
                format!(
                    "attempt {attempt}/{max_attempts} | asking {} to select one action from {} configured candidate(s)",
                    self.config.model,
                    envelope.candidates.len(),
                ),
            );

            let response_text = match self.query_ollama(&prompt).await {
                Ok(response) => response,
                Err(err) if attempt < max_attempts => {
                    self.log_decision_trace(
                        "retry",
                        format!(
                            "attempt {attempt}/{max_attempts} | model request failed; retrying: {err:#}"
                        ),
                    );
                    warn!(
                        attempt,
                        max_attempts,
                        reason = %format!("{err:#}"),
                        "anomaly recovery request failed; retrying"
                    );
                    feedback = None;
                    continue;
                }
                Err(err) => {
                    self.log_decision_trace(
                        "failure",
                        format!(
                            "attempt {attempt}/{max_attempts} | model request failed with no retries left: {err:#}"
                        ),
                    );
                    return Err(anyhow!(
                        "attempt {attempt}/{max_attempts} query failed: {err:#}"
                    ));
                }
            };

            warn!("LLM response: {response_text}");

            let decision = match self.parse_advisor_decision(&response_text) {
                Ok(decision) => {
                    self.log_decision_trace(
                        "response",
                        format!(
                            "attempt {attempt}/{max_attempts} | model selected {} -> {} | rationale: {}",
                            decision.anomaly_id, decision.action_id, decision.reason,
                        ),
                    );
                    decision
                }
                Err(err) if attempt < max_attempts => {
                    self.log_decision_trace(
                        "repair",
                        format!(
                            "attempt {attempt}/{max_attempts} | model reply was not valid decision JSON; requesting repair: {err:#}"
                        ),
                    );
                    warn!(
                        attempt,
                        max_attempts,
                        reason = %format!("{err:#}"),
                        "anomaly recovery response failed parsing; requesting repair"
                    );
                    feedback = Some(self.build_repair_feedback(
                        "parse_advisor_decision",
                        &err,
                        &response_text,
                    ));
                    continue;
                }
                Err(err) => {
                    self.log_decision_trace(
                        "failure",
                        format!(
                            "attempt {attempt}/{max_attempts} | model reply was not valid decision JSON: {err:#}"
                        ),
                    );
                    return Err(err);
                }
            };

            if let Err(err) = self.evaluate_decision(&decision, &envelope.candidates) {
                if attempt < max_attempts {
                    self.log_decision_trace(
                        "repair",
                        format!(
                            "attempt {attempt}/{max_attempts} | selection was outside the configured candidates or actions; requesting repair: {err:#}"
                        ),
                    );
                    warn!(
                        attempt,
                        max_attempts,
                        reason = %format!("{err:#}"),
                        "anomaly recovery response failed validation; requesting repair"
                    );
                    feedback =
                        Some(self.build_repair_feedback("evaluate_decision", &err, &response_text));
                    continue;
                }
                self.log_decision_trace(
                    "failure",
                    format!("attempt {attempt}/{max_attempts} | selection was rejected: {err:#}"),
                );
                return Err(err);
            }

            self.log_decision_trace(
                "validation",
                format!(
                    "attempt {attempt}/{max_attempts} | accepted {} -> {}; evidence path is allowed",
                    decision.anomaly_id, decision.action_id,
                ),
            );

            info!(
                attempt,
                max_attempts,
                anomaly_id = %decision.anomaly_id,
                action_id = %decision.action_id,
                "anomaly recovery selected a configured anomaly action"
            );
            return Ok(decision);
        }

        Err(anyhow!("anomaly recovery exhausted decision attempts"))
    }

    fn build_ollama_request_body(&self, prompt: &str) -> Result<String> {
        #[derive(Serialize)]
        struct OllamaGenerateRequest<'a> {
            model: &'a str,
            prompt: &'a str,
            stream: bool,
            format: &'a Value,
            options: OllamaOptions,
        }
        #[derive(Serialize)]
        struct OllamaOptions {
            temperature: f64,
            num_predict: u32,
        }

        let format = decision_response_schema();
        Ok(serde_json::to_string(&OllamaGenerateRequest {
            model: self.config.model.as_str(),
            prompt,
            stream: false,
            format: &format,
            options: OllamaOptions {
                temperature: self.config.response_temperature,
                num_predict: self.config.num_predict,
            },
        })?)
    }

    async fn query_ollama(&self, prompt: &str) -> Result<String> {
        let body = self.build_ollama_request_body(prompt)?;
        let url = format!(
            "http://{}:{}{}",
            self.config.ollama_host, self.config.ollama_port, self.config.ollama_path
        );
        let request_start = Instant::now();

        info!(
            host = %self.config.ollama_host,
            port = self.config.ollama_port,
            path = %self.config.ollama_path,
            model = %self.config.model,
            timeout_ms = self.config.request_timeout_ms,
            prompt_chars = prompt.chars().count(),
            "anomaly recovery sending constrained ollama request"
        );

        let request_future = async {
            let response = reqwest::Client::new()
                .post(&url)
                .header(CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
                .map_err(|e| anyhow!("ollama HTTP request failed: {e}"))?;
            let status = response.status();
            let body_text = response
                .text()
                .await
                .map_err(|e| anyhow!("failed reading ollama response body: {e}"))?;
            if !status.is_success() {
                return Err(anyhow!(
                    "ollama returned HTTP status {}: {}",
                    status,
                    clip_chars(&body_text, 400)
                ));
            }
            Result::<String>::Ok(body_text)
        };

        let body_text = match timeout(
            Duration::from_millis(self.config.request_timeout_ms),
            request_future,
        )
        .await
        {
            Ok(Ok(body)) => body,
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err(anyhow!("ollama request timed out")),
        };

        info!(
            elapsed_ms = request_start.elapsed().as_millis() as u64,
            response_chars = body_text.chars().count(),
            "anomaly recovery completed ollama request"
        );

        let response: OllamaResponse = serde_json::from_str(&body_text)
            .map_err(|e| anyhow!("invalid ollama JSON payload: {e}"))?;
        if response.done_reason.as_deref() == Some("length") {
            return Err(anyhow!("ollama response stopped at token limit"));
        }
        debug!(
            done = response.done,
            done_reason = ?response.done_reason,
            eval_count = ?response.eval_count,
            "anomaly recovery parsed ollama completion metadata"
        );

        let response_text = response.response.trim();
        if response_text.is_empty() {
            return Err(anyhow!("ollama response was empty"));
        }
        if response_text.chars().count() > self.config.max_response_chars {
            return Err(anyhow!(
                "ollama response exceeded max_response_chars ({})",
                self.config.max_response_chars
            ));
        }
        Ok(response_text.to_string())
    }

    fn parse_advisor_decision(&self, response_text: &str) -> Result<AdvisorDecision> {
        serde_json::from_str(response_text.trim())
            .map_err(|e| anyhow!("could not parse strict advisor decision JSON: {e}"))
    }

    fn evaluate_decision<'a>(
        &self,
        decision: &AdvisorDecision,
        candidates: &'a [AnomalyCandidate],
    ) -> Result<(&'a AnomalyCandidate, AllowedAction)> {
        if decision.anomaly_id.trim().is_empty() || decision.action_id.trim().is_empty() {
            return Err(anyhow!("anomaly_id and action_id must not be empty"));
        }
        if decision.reason.trim().is_empty() {
            return Err(anyhow!("reason must not be empty"));
        }

        let candidate = candidates
            .iter()
            .find(|candidate| {
                candidate.anomaly_id == decision.anomaly_id
                    || candidate.rule_id == decision.anomaly_id
            })
            .ok_or_else(|| {
                let supplied = candidates
                    .iter()
                    .map(|candidate| candidate.anomaly_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow!(
                    "anomaly_id '{}' was not supplied; choose one of [{supplied}]",
                    decision.anomaly_id
                )
            })?;
        if decision.evidence_paths != vec![candidate.path.clone()] {
            return Err(anyhow!(
                "evidence_paths must exactly equal ['{}'] for anomaly '{}'",
                candidate.path,
                candidate.anomaly_id
            ));
        }

        let action = candidate
            .eligible_actions
            .iter()
            .copied()
            .find(|action| action.as_str() == decision.action_id)
            .ok_or_else(|| {
                anyhow!(
                    "action_id '{}' is not eligible for anomaly '{}'",
                    decision.action_id,
                    candidate.anomaly_id
                )
            })?;
        if self.config.action_definition(action).is_none() {
            return Err(anyhow!(
                "action_id '{}' is not in the action catalog",
                decision.action_id
            ));
        }
        Ok((candidate, action))
    }

    fn evaluate_rule(rule: &NominalRule, observed: &Value) -> RuleEvaluation {
        match rule.kind {
            NominalRuleKind::NumberRange => {
                let Some(value) = observed.as_f64() else {
                    return RuleEvaluation::Invalid("expected a number".to_string());
                };
                let below_minimum = rule.min.is_some_and(|minimum| value < minimum);
                let above_maximum = rule.max.is_some_and(|maximum| value > maximum);
                if below_minimum || above_maximum {
                    RuleEvaluation::Violation
                } else {
                    RuleEvaluation::Normal
                }
            }
            NominalRuleKind::Enum => {
                let Some(value) = observed.as_str() else {
                    return RuleEvaluation::Invalid("expected a string".to_string());
                };
                if rule.allowed.iter().any(|allowed| allowed == value) {
                    RuleEvaluation::Normal
                } else {
                    RuleEvaluation::Violation
                }
            }
            NominalRuleKind::Boolean => {
                let Some(value) = observed.as_bool() else {
                    return RuleEvaluation::Invalid("expected a boolean".to_string());
                };
                if Some(value) == rule.expected {
                    RuleEvaluation::Normal
                } else {
                    RuleEvaluation::Violation
                }
            }
            NominalRuleKind::Required => {
                if observed.is_null() {
                    RuleEvaluation::Invalid("field must not be null".to_string())
                } else {
                    RuleEvaluation::Normal
                }
            }
        }
    }

    fn evaluate_static_profile(&mut self, telemetry: &TelemetrySample) {
        self.current_candidates.clear();

        let Some(source) = telemetry.source.as_deref() else {
            self.last_plan_signature = None;
            warn!(
                ts_mono = telemetry.ts_mono,
                "anomaly recovery received telemetry without a source"
            );
            return;
        };
        let Some(profile) = self.config.profile_for_source(source).cloned() else {
            self.last_plan_signature = None;
            warn!(
                source,
                ts_mono = telemetry.ts_mono,
                "anomaly recovery has no nominal profile for telemetry source"
            );
            return;
        };

        for rule in &profile.rules {
            let state_key = format!("{}:{}:{}", profile.id, source, rule.id);
            let Some(observed) = value_at_payload_path(&telemetry.payload, &rule.path) else {
                self.rule_states
                    .entry(state_key)
                    .or_default()
                    .consecutive_violations = 0;
                // warn!(
                //     profile = %profile.id,
                //     source,
                //     rule = %rule.id,
                //     path = %rule.path,
                //     "configured telemetry field is missing; no action will be emitted"
                // );
                continue;
            };

            match Self::evaluate_rule(rule, observed) {
                RuleEvaluation::Normal => {
                    self.rule_states
                        .entry(state_key)
                        .or_default()
                        .consecutive_violations = 0;
                }
                RuleEvaluation::Invalid(reason) => {
                    self.rule_states
                        .entry(state_key)
                        .or_default()
                        .consecutive_violations = 0;
                    warn!(
                        profile = %profile.id,
                        source,
                        rule = %rule.id,
                        path = %rule.path,
                        reason = %reason,
                        "configured telemetry field is invalid; no action will be emitted"
                    );
                }
                RuleEvaluation::Violation => {
                    let consecutive_violations = {
                        let state = self.rule_states.entry(state_key).or_default();
                        state.consecutive_violations += 1;
                        state.consecutive_violations
                    };
                    if consecutive_violations >= rule.min_consecutive_samples {
                        self.current_candidates.push(AnomalyCandidate {
                            profile_id: profile.id.clone(),
                            rule_id: rule.id.clone(),
                            anomaly_id: format!("{}-{}", profile.id, rule.id),
                            source: source.to_string(),
                            ts_mono: telemetry.ts_mono,
                            path: rule.path.clone(),
                            observed: observed.clone(),
                            expectation: rule.expectation_description(),
                            severity: rule.severity,
                            eligible_actions: rule.eligible_actions.clone(),
                        });
                    }
                }
            }
        }

        self.current_candidates.sort_by(|left, right| {
            right
                .severity
                .cmp(&left.severity)
                .then_with(|| left.rule_id.cmp(&right.rule_id))
        });
        if self.current_candidates.is_empty() {
            self.last_plan_signature = None;
        }
    }

    fn candidate_signature(candidates: &[AnomalyCandidate]) -> String {
        let mut ids = candidates
            .iter()
            .map(|candidate| format!("{}:{}", candidate.source, candidate.anomaly_id))
            .collect::<Vec<_>>();
        ids.sort();
        ids.join("|")
    }

    async fn emit_action(
        &self,
        runtime: &mut ModeRuntime,
        candidate: &AnomalyCandidate,
        action: AllowedAction,
        reason: &str,
    ) -> Result<()> {
        let command = command_for_action(action)?;
        runtime
            .command(CommandEnvelope {
                from: runtime.mode_id(),
                cmd: TimedCommand::Now(command),
            })
            .await?;
        info!(
            profile_id = %candidate.profile_id,
            anomaly_id = %candidate.anomaly_id,
            source = %candidate.source,
            ts_mono = candidate.ts_mono,
            action_id = action.as_str(),
            evidence_path = %candidate.path,
            reason = %reason,
            "anomaly recovery emitted profile-backed anomaly action"
        );
        self.log_decision_trace(
            "proposal",
            format!(
                "submitted {} for {} ({}) to the SAFE command board",
                action.as_str(),
                candidate.anomaly_id,
                candidate.path,
            ),
        );
        Ok(())
    }

    async fn plan_current_candidates(&mut self, runtime: &mut ModeRuntime) -> Result<()> {
        if self.config.require_board_snapshot && !self.has_board_snapshot {
            if !self.warned_missing_board_snapshot {
                warn!("anomaly recovery waiting for initial board snapshot before planning");
                self.warned_missing_board_snapshot = true;
            }
            return Ok(());
        }
        if self.current_candidates.is_empty() {
            return Ok(());
        }

        let signature = Self::candidate_signature(&self.current_candidates);
        if self.last_plan_signature.as_deref() == Some(signature.as_str()) {
            return Ok(());
        }

        let actionable = self
            .current_candidates
            .iter()
            .filter(|candidate| !candidate.eligible_actions.is_empty())
            .cloned()
            .collect::<Vec<_>>();

        if self.config.decision_trace {
            self.log_decision_trace(
                "candidates",
                format!(
                    "{} detected candidate(s); {} have configured actions",
                    self.current_candidates.len(),
                    actionable.len(),
                ),
            );
            for candidate in &actionable {
                let actions = candidate
                    .eligible_actions
                    .iter()
                    .map(|action| action.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                self.log_decision_trace(
                    "candidate",
                    format!(
                        "{} | {}={} | expected {} | actions: {}",
                        candidate.anomaly_id,
                        candidate.path,
                        clip_chars(&candidate.observed.to_string(), 120),
                        candidate.expectation,
                        actions,
                    ),
                );
            }
        }
        if actionable.is_empty() {
            warn!(
                candidate_count = self.current_candidates.len(),
                "nominal-profile anomalies have no eligible action; no command will be emitted"
            );
            self.last_plan_signature = Some(signature);
            return Ok(());
        }

        if actionable.len() == 1 && actionable[0].eligible_actions.len() == 1 {
            let candidate = &actionable[0];
            let action = candidate.eligible_actions[0];
            self.log_decision_trace(
                "decision",
                format!(
                    "LLM skipped | {} has exactly one configured action: {}",
                    candidate.anomaly_id,
                    action.as_str(),
                ),
            );
            self.emit_action(
                runtime,
                candidate,
                action,
                "single configured eligible action",
            )
            .await?;
            self.last_plan_signature = Some(signature);
            return Ok(());
        }

        let envelope = self.build_decision_envelope(actionable);
        let decision = self.plan_decision_with_feedback_loop(&envelope).await?;
        let (candidate, action) = self.evaluate_decision(&decision, &envelope.candidates)?;
        self.emit_action(runtime, candidate, action, &decision.reason)
            .await?;
        self.last_plan_signature = Some(signature);
        Ok(())
    }
}

fn command_for_action(action: AllowedAction) -> Result<Command> {
    match action {
        AllowedAction::PointNadir => Ok(Command::PointNadir),
        AllowedAction::PointSunYaw => Ok(Command::PointSunYaw),
        AllowedAction::ThrusterOff => Ok(Command::ThrusterOff),
        AllowedAction::CaptureImage | AllowedAction::Noop => Err(anyhow!(
            "action '{}' is not supported for anomaly recommendations",
            action.as_str()
        )),
    }
}

fn value_at_payload_path<'a>(payload: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(payload, |value, segment| match value {
            Value::Object(values) => values.get(segment),
            Value::Array(values) => segment
                .parse::<usize>()
                .ok()
                .and_then(|index| values.get(index)),
            _ => None,
        })
}

fn decision_response_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["anomaly_id", "action_id", "reason", "evidence_paths"],
        "properties": {
            "anomaly_id": {"type": "string"},
            "action_id": {"type": "string"},
            "reason": {"type": "string"},
            "evidence_paths": {
                "type": "array",
                "minItems": 1,
                "items": {"type": "string"}
            }
        }
    })
}

fn clip_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    input.chars().take(max_chars).collect()
}

fn trace_text(input: &str) -> String {
    let mut output = String::new();
    for character in input.chars().take(1_000) {
        match character {
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                output.push_str(&format!("\\u{{{:x}}}", character as u32));
            }
            character => output.push(character),
        }
    }
    output
}

#[async_trait]
impl ModeHandler<LlmAdvisorModeConfig> for LlmAdvisorMode {
    fn set_config(&mut self, config: LlmAdvisorModeConfig) -> Result<()> {
        config.validate()?;
        info!(
            host = %config.ollama_host,
            port = config.ollama_port,
            path = %config.ollama_path,
            model = %config.model,
            timeout_ms = config.request_timeout_ms,
            profiles = config.nominal_profiles.len(),
            actions = config.action_catalog.len(),
            "anomaly recovery static nominal profile config loaded"
        );
        self.config = config;
        self.latest_telemetry = None;
        self.current_candidates.clear();
        self.rule_states.clear();
        self.has_board_snapshot = false;
        self.last_plan_signature = None;
        self.warned_missing_board_snapshot = false;
        Ok(())
    }

    async fn on_activate(&mut self, runtime: &mut ModeRuntime) -> Result<()> {
        if let Err(err) = self.plan_current_candidates(runtime).await {
            self.log_planning_error(
                "on_activate",
                "plan_current_candidates",
                &err,
                self.latest_telemetry.as_ref(),
            );
        }
        Ok(())
    }

    async fn on_deactivate(&mut self, _runtime: &mut ModeRuntime) -> Result<()> {
        self.last_plan_signature = None;
        Ok(())
    }

    async fn on_telemetry(
        &mut self,
        runtime: &mut ModeRuntime,
        telemetry: TelemetryFrame,
    ) -> Result<()> {
        let sample = TelemetrySample {
            source: telemetry.source,
            ts_mono: telemetry.ts_mono,
            payload: telemetry.payload,
        };
        self.latest_telemetry = Some(sample.clone());
        self.evaluate_static_profile(&sample);

        if !runtime.is_active() {
            return Ok(());
        }
        if let Err(err) = self.plan_current_candidates(runtime).await {
            self.log_planning_error(
                "on_telemetry",
                "plan_current_candidates",
                &err,
                Some(&sample),
            );
        }
        Ok(())
    }

    async fn on_board_snapshot(
        &mut self,
        runtime: &mut ModeRuntime,
        board: AutonomyModeBoardState,
    ) -> Result<()> {
        let first_snapshot = !self.has_board_snapshot;
        self.has_board_snapshot = true;
        self.latest_board_snapshot = board;
        if first_snapshot
            && runtime.is_active()
            && let Err(err) = self.plan_current_candidates(runtime).await
        {
            self.log_planning_error(
                "on_board_snapshot",
                "plan_current_candidates",
                &err,
                self.latest_telemetry.as_ref(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE_FIXTURE: &str = include_str!("../testdata/static_nominal_profile.json");
    const TELEMETRY_FIXTURE: &str = include_str!("../testdata/static_nominal_telemetry.jsonl");

    fn configured_mode() -> LlmAdvisorMode {
        let config = serde_json::from_str(PROFILE_FIXTURE).expect("fixture should parse");
        let mut mode = LlmAdvisorMode::new();
        mode.set_config(config).expect("fixture should validate");
        mode
    }

    fn sample(ts_mono: u64, payload: Value) -> TelemetrySample {
        TelemetrySample {
            source: Some("example".to_string()),
            ts_mono,
            payload,
        }
    }

    #[test]
    fn numeric_rule_requires_configured_persistence() {
        let mut mode = configured_mode();
        let high_temperature = serde_json::json!({"telemetry": {"temperature_c": 50.0, "mode": "nominal", "enabled": true}});

        mode.evaluate_static_profile(&sample(1, high_temperature.clone()));
        assert!(mode.current_candidates.is_empty());

        mode.evaluate_static_profile(&sample(2, high_temperature));
        assert_eq!(mode.current_candidates.len(), 1);
        assert_eq!(
            mode.current_candidates[0].rule_id,
            "temperature_out_of_nominal"
        );
        assert_eq!(
            mode.current_candidates[0].anomaly_id,
            "example-v1-temperature_out_of_nominal"
        );
        assert_eq!(mode.current_candidates[0].observed, serde_json::json!(50.0));
        assert_eq!(
            mode.current_candidates[0].eligible_actions,
            vec![AllowedAction::PointSunYaw]
        );
    }

    #[test]
    fn generic_fixture_reaches_static_profile_anomalies() {
        let mut mode = configured_mode();
        for frame in TELEMETRY_FIXTURE.lines() {
            let frame: Value = serde_json::from_str(frame).expect("telemetry fixture line is JSON");
            mode.evaluate_static_profile(&TelemetrySample {
                source: frame["source"].as_str().map(str::to_string),
                ts_mono: frame["ts_mono"].as_u64().expect("fixture timestamp"),
                payload: frame["payload"].clone(),
            });
        }

        let ids = mode
            .current_candidates
            .iter()
            .map(|candidate| candidate.rule_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                "temperature_out_of_nominal",
                "mode_invalid",
                "enabled_unexpected"
            ]
        );
    }

    #[test]
    fn normal_telemetry_clears_anomaly_episode() {
        let mut mode = configured_mode();
        let high_temperature = serde_json::json!({"telemetry": {"temperature_c": 50.0, "mode": "nominal", "enabled": true}});
        mode.evaluate_static_profile(&sample(1, high_temperature.clone()));
        mode.evaluate_static_profile(&sample(2, high_temperature));
        assert_eq!(mode.current_candidates.len(), 1);

        mode.evaluate_static_profile(&sample(
            3,
            serde_json::json!({"telemetry": {"temperature_c": 20.0, "mode": "nominal", "enabled": true}}),
        ));
        assert!(mode.current_candidates.is_empty());

        mode.evaluate_static_profile(&sample(
            4,
            serde_json::json!({"telemetry": {"temperature_c": 50.0, "mode": "nominal", "enabled": true}}),
        ));
        assert!(mode.current_candidates.is_empty());
    }

    #[test]
    fn unprofiled_or_invalid_telemetry_never_becomes_a_candidate() {
        let mut mode = configured_mode();
        mode.evaluate_static_profile(&TelemetrySample {
            source: Some("other".to_string()),
            ts_mono: 1,
            payload: serde_json::json!({"telemetry": {"temperature_c": 50.0}}),
        });
        assert!(mode.current_candidates.is_empty());

        mode.evaluate_static_profile(&sample(
            2,
            serde_json::json!({"telemetry": {"temperature_c": "hot", "mode": "nominal", "enabled": true}}),
        ));
        assert!(mode.current_candidates.is_empty());
    }

    #[test]
    fn enum_and_boolean_rules_become_candidates() {
        let mut mode = configured_mode();
        let payload = serde_json::json!({"telemetry": {"temperature_c": 20.0, "mode": "unknown", "enabled": false}});

        mode.evaluate_static_profile(&sample(1, payload));
        let ids = mode
            .current_candidates
            .iter()
            .map(|candidate| candidate.rule_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["mode_invalid", "enabled_unexpected"]);
    }

    #[test]
    fn strict_parser_rejects_wrapped_text() {
        let mode = configured_mode();
        assert!(mode
            .parse_advisor_decision(
                "preface {\"anomaly_id\":\"mode_invalid\",\"action_id\":\"point_nadir\",\"reason\":\"x\",\"evidence_paths\":[\"telemetry.mode\"]}"
            )
            .is_err());
    }

    #[test]
    fn decision_trace_text_is_single_line_and_bounded() {
        let input = format!("line one\nline two\t{}", "x".repeat(1_000));
        let trace = trace_text(&input);
        assert_eq!(trace, format!("line one\\nline two\\t{}", "x".repeat(982)));
        assert!(!trace.contains('\n'));
    }

    #[test]
    fn payload_paths_support_object_keys_and_array_indexes() {
        let payload = serde_json::json!({"sensors": [{"temperature_c": 21.0}]});
        assert_eq!(
            value_at_payload_path(&payload, "sensors.0.temperature_c"),
            Some(&serde_json::json!(21.0))
        );
        assert!(value_at_payload_path(&payload, "sensors.1.temperature_c").is_none());
    }

    #[test]
    fn decision_must_select_configured_candidate_action_and_evidence() {
        let mut mode = configured_mode();
        mode.evaluate_static_profile(&sample(
            1,
            serde_json::json!({"telemetry": {"temperature_c": 20.0, "mode": "unknown", "enabled": true}}),
        ));
        let candidates = mode
            .current_candidates
            .iter()
            .filter(|candidate| !candidate.eligible_actions.is_empty())
            .cloned()
            .collect::<Vec<_>>();
        let valid = AdvisorDecision {
            anomaly_id: candidates[0].anomaly_id.clone(),
            action_id: "point_nadir".to_string(),
            reason: "Configured mode rule is violated.".to_string(),
            evidence_paths: vec!["telemetry.mode".to_string()],
        };
        assert!(mode.evaluate_decision(&valid, &candidates).is_ok());

        let legacy_rule_id = AdvisorDecision {
            anomaly_id: "mode_invalid".to_string(),
            ..valid.clone()
        };
        assert!(mode.evaluate_decision(&legacy_rule_id, &candidates).is_ok());

        let invalid = AdvisorDecision {
            action_id: "thruster_off".to_string(),
            ..valid
        };
        assert!(mode.evaluate_decision(&invalid, &candidates).is_err());
    }

    #[test]
    fn prompt_contains_only_profile_backed_candidates() {
        let mut mode = configured_mode();
        mode.evaluate_static_profile(&sample(
            1,
            serde_json::json!({"telemetry": {"temperature_c": 20.0, "mode": "unknown", "enabled": true}}),
        ));
        let candidates = mode
            .current_candidates
            .iter()
            .filter(|candidate| !candidate.eligible_actions.is_empty())
            .cloned()
            .collect::<Vec<_>>();
        let prompt = mode
            .build_prompt(&mode.build_decision_envelope(candidates), None)
            .expect("prompt should build");
        assert!(prompt.contains("example-v1-mode_invalid"));
        assert!(prompt.contains("eligible_actions"));
        assert!(!prompt.contains("telemetry_latest"));
    }

    #[test]
    fn ollama_request_uses_json_schema_format() {
        let mode = configured_mode();
        let body = mode
            .build_ollama_request_body("test prompt")
            .expect("request should serialize");
        let value: Value = serde_json::from_str(&body).expect("request should be JSON");
        assert_eq!(value["format"]["required"][0], "anomaly_id");
        assert_eq!(value["options"]["temperature"], 0.0);
    }
}
