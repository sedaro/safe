use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, ensure};
use async_trait::async_trait;
use safe::mode_runtime::{ModeHandler, ModeRuntime};
use safe::protocol::AutonomyModeBoardState;
use safe::telemetry_frame::TelemetryFrame;
use serde_json::Value;
use tracing::{info, warn};

use crate::config::{AllowedAction, AnomalyRecoveryModeConfig, NominalRule, NominalRuleKind};
use crate::types::{AnomalyCandidate, AnomalyRecoveryMode, TelemetrySample};

enum RuleEvaluation {
    Normal,
    Violation,
    Invalid(String),
}

impl AnomalyRecoveryMode {
    fn log_decision_trace(&self, stage: &str, detail: impl std::fmt::Display) {
        if self.config.observability.decision_trace {
            let detail = trace_text(
                &detail.to_string(),
                self.config.observability.trace_max_chars,
            );
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
            .map(|candidate| {
                format!(
                    "{}:{}:{}:{}",
                    candidate.source, candidate.anomaly_id, candidate.ts_mono, candidate.observed
                )
            })
            .collect::<Vec<_>>();
        ids.sort();
        ids.join("|")
    }

    fn planning_in_flight(&self) -> bool {
        self.planning_task
            .as_ref()
            .is_some_and(|task| !task.is_finished())
    }

    fn defer_telemetry_while_planning(&mut self, sample: TelemetrySample) -> bool {
        if !self.planning_in_flight() {
            return false;
        }
        self.pending_telemetry = Some(sample);
        true
    }

    fn stop_planning(&mut self) {
        self.shutdown_intent = None;
        self.planning_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if let Some(cancel) = self.planning_cancel.take() {
            cancel.cancel();
        }
        if let Some(task) = self.planning_task.take() {
            task.abort();
        }
    }

    async fn reap_finished_plan(&mut self) {
        let Some(task) = self.planning_task.take_if(|task| task.is_finished()) else {
            return;
        };
        match task.await {
            Ok(Ok(intent)) => {
                self.next_plan_retry = None;
                self.shutdown_intent = intent;
            }
            Ok(Err(error)) => {
                warn!(reason = %error, "anomaly recovery planning task failed");
                if self.config.replanning.failed_plan_retry_ms > 0 {
                    self.next_plan_retry = Some(
                        Instant::now()
                            + Duration::from_millis(self.config.replanning.failed_plan_retry_ms),
                    );
                }
            }
            Err(error) if !error.is_cancelled() => {
                warn!(reason = %error, "anomaly recovery planning task failed");
            }
            Err(_) => {}
        }
        self.planning_cancel = None;
    }

    fn validate_shutdown_intent(&self, intent: &crate::actions::ShutdownIntent) -> Result<()> {
        use std::sync::atomic::Ordering;
        ensure!(
            self.active.load(Ordering::Acquire),
            "shutdown mode is inactive"
        );
        ensure!(
            self.planning_generation.load(Ordering::Acquire) == intent.generation,
            "shutdown investigation was superseded"
        );
        // Telemetry is coalesced during planning; it must also invalidate local
        // execution, even before the queued sample is evaluated by the handler.
        ensure!(
            self.pending_telemetry.is_none(),
            "shutdown has newer pending telemetry"
        );
        let current = self.live_context.snapshot();
        ensure!(
            current.telemetry_version == intent.telemetry_version
                && current.board_version == intent.board_version,
            "shutdown evidence is stale"
        );
        let board = current
            .board
            .as_ref()
            .ok_or_else(|| anyhow!("shutdown requires a known board"))?;
        ensure!(
            board.proposals.is_empty()
                && board.approved.is_empty()
                && board.source_of_truth.is_empty(),
            "shutdown simulation cannot omit existing board commands"
        );
        ensure!(
            self.current_candidates
                .iter()
                .any(|c| c.anomaly_id == intent.anomaly_id
                    && c.eligible_actions.contains(&AllowedAction::Shutdown)),
            "shutdown candidate is no longer eligible"
        );
        Ok(())
    }

    async fn execute_pending_shutdown(&mut self, directory: &std::path::Path) {
        let Some(intent) = self.shutdown_intent.take() else {
            return;
        };
        if let Err(error) = self.validate_shutdown_intent(&intent) {
            warn!(reason = %error, "discarding obsolete shutdown intent");
            return;
        }
        if let Err(error) = self
            .shutdown_controller
            .execute(&intent, directory, &self.config.shutdown_command)
            .await
        {
            warn!(assessment_id = %intent.assessment_id, reason = %format!("{error:#}"),
                "anomaly recovery shutdown attempt failed or was suppressed");
        }
    }

    async fn accept_telemetry(
        &mut self,
        runtime: &mut ModeRuntime,
        sample: TelemetrySample,
    ) -> Result<()> {
        self.latest_telemetry = Some(sample.clone());
        self.live_context.update_telemetry(
            sample.clone(),
            self.config.evidence.history_samples_per_source,
        );
        self.evaluate_static_profile(&sample);

        if runtime.is_active() {
            self.plan_current_candidates(runtime).await?;
        }
        Ok(())
    }

    async fn plan_current_candidates(&mut self, runtime: &mut ModeRuntime) -> Result<()> {
        if self.config.replanning.require_board_snapshot && !self.has_board_snapshot {
            if !self.warned_missing_board_snapshot {
                warn!("anomaly recovery waiting for initial board snapshot before planning");
                self.warned_missing_board_snapshot = true;
            }
            return Ok(());
        }
        if self.current_candidates.is_empty() {
            self.planning_generation
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            if let Some(cancel) = self.planning_cancel.take() {
                cancel.cancel();
            }
            return Ok(());
        }

        let signature = Self::candidate_signature(&self.current_candidates);
        if self.last_plan_signature.as_deref() == Some(signature.as_str()) {
            return Ok(());
        }

        if self.config.observability.decision_trace {
            self.log_decision_trace(
                "candidates",
                format!(
                    "{} detected candidate(s); {} have configured actions",
                    self.current_candidates.len(),
                    self.current_candidates
                        .iter()
                        .filter(|candidate| !candidate.eligible_actions.is_empty())
                        .count(),
                ),
            );
            for candidate in &self.current_candidates {
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
                        clip_chars(
                            &candidate.observed.to_string(),
                            self.config.observability.candidate_value_max_chars,
                        ),
                        candidate.expectation,
                        actions,
                    ),
                );
            }
        }
        if let Some(cancel) = self.planning_cancel.take() {
            cancel.cancel();
        }
        let cancel = safe_sim::CancellationToken::new();
        let generation = self
            .planning_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            + 1;
        let telemetry = self
            .latest_telemetry
            .clone()
            .ok_or_else(|| anyhow!("missing telemetry snapshot"))?;
        let request = crate::planner::PlanningRequest {
            config: self.config.clone(),
            candidates: self.current_candidates.clone(),
            telemetry,
            live_context: self.live_context.clone(),
            mode_id: runtime.mode_id(),
            generation,
            generations: self.planning_generation.clone(),
            active: self.active.clone(),
            cancel: cancel.clone(),
            adapter: self
                .adapter
                .clone()
                .ok_or_else(|| anyhow!("anomaly recovery LLM adapter has not been configured"))?,
            telemetry_version: self.live_context.snapshot().telemetry_version,
            board_version: self.live_context.snapshot().board_version,
        };
        let output = runtime.output_tx();
        self.planning_task = Some(tokio::spawn(async move {
            crate::planner::run(request, output).await
        }));
        self.planning_cancel = Some(cancel);
        self.last_plan_signature = Some(signature);
        self.next_plan_retry = None;
        Ok(())
    }
}

pub(crate) fn value_at_payload_path<'a>(payload: &'a Value, path: &str) -> Option<&'a Value> {
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

fn clip_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    input.chars().take(max_chars).collect()
}

fn trace_text(input: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for character in input.chars().take(max_chars) {
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
impl ModeHandler<AnomalyRecoveryModeConfig> for AnomalyRecoveryMode {
    fn set_config(&mut self, config: AnomalyRecoveryModeConfig) -> Result<()> {
        config.validate()?;
        if let Some(recovery) = config.recovery.clone() {
            self.stop_planning();
            self.adapter = None;
            self.recovery_runtime = Some(crate::recovery_runtime::RecoveryRuntime::new(recovery));
            self.config = config;
            return Ok(());
        }
        self.recovery_runtime = None;
        let adapter = self
            .adapter_registry
            .build(&config.llm.adapter)
            .map_err(|error| anyhow!("could not initialize configured LLM adapter: {error}"))?;
        info!(
            adapter = adapter.kind(),
            model = %config.llm.model,
            timeout_ms = config.llm.request_timeout_ms,
            profiles = config.nominal_profiles.len(),
            actions = config.action_catalog.len(),
            "anomaly recovery static nominal profile config loaded"
        );
        self.config = config;
        self.adapter = Some(adapter.into());
        self.latest_telemetry = None;
        self.pending_telemetry = None;
        self.live_context.clear();
        self.current_candidates.clear();
        self.rule_states.clear();
        self.has_board_snapshot = false;
        self.last_plan_signature = None;
        self.warned_missing_board_snapshot = false;
        self.next_plan_retry = None;
        self.active
            .store(false, std::sync::atomic::Ordering::Release);
        self.stop_planning();
        Ok(())
    }

    async fn on_activate(&mut self, runtime: &mut ModeRuntime) -> Result<()> {
        if let Some(recovery) = &mut self.recovery_runtime {
            recovery.activate();
            return recovery
                .process(runtime, &self.config, &self.adapter_registry, None)
                .await;
        }
        self.active
            .store(true, std::sync::atomic::Ordering::Release);
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
        self.active
            .store(false, std::sync::atomic::Ordering::Release);
        self.stop_planning();
        self.pending_telemetry = None;
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
        if self.recovery_runtime.is_some() {
            if self.config.advisory.enabled
                && sample
                    .source
                    .as_deref()
                    .is_some_and(|source| self.config.profile_for_source(source).is_some())
            {
                self.evaluate_static_profile(&sample);
                self.recovery_runtime
                    .as_mut()
                    .unwrap()
                    .note_candidates(&self.current_candidates);
            }
            return self
                .recovery_runtime
                .as_mut()
                .unwrap()
                .process(runtime, &self.config, &self.adapter_registry, Some(&sample))
                .await;
        }
        self.reap_finished_plan().await;
        if runtime.is_active() && self.defer_telemetry_while_planning(sample.clone()) {
            return Ok(());
        }
        self.pending_telemetry = None;
        if let Err(err) = self.accept_telemetry(runtime, sample.clone()).await {
            self.log_planning_error("on_telemetry", "accept_telemetry", &err, Some(&sample));
        }
        Ok(())
    }

    async fn on_board_snapshot(
        &mut self,
        runtime: &mut ModeRuntime,
        board: AutonomyModeBoardState,
    ) -> Result<()> {
        if let Some(recovery) = &mut self.recovery_runtime {
            recovery.note_board(&board);
            return Ok(());
        }
        self.has_board_snapshot = true;
        self.live_context.update_board(board.clone());
        self.latest_board_snapshot = board;
        if self.config.replanning.replan_on_board_change {
            self.stop_planning();
            self.last_plan_signature = None;
            if runtime.is_active()
                && let Err(err) = self.plan_current_candidates(runtime).await
            {
                self.log_planning_error(
                    "on_board_snapshot",
                    "plan_current_candidates",
                    &err,
                    self.latest_telemetry.as_ref(),
                );
            }
        }
        Ok(())
    }

    async fn on_tick(&mut self, runtime: &mut ModeRuntime) -> Result<()> {
        if let Some(recovery) = &mut self.recovery_runtime {
            return recovery
                .process(runtime, &self.config, &self.adapter_registry, None)
                .await;
        }
        self.reap_finished_plan().await;
        self.execute_pending_shutdown(runtime.working_directory())
            .await;
        if !runtime.is_active() || self.planning_in_flight() {
            return Ok(());
        }
        if let Some(sample) = self.pending_telemetry.take() {
            self.accept_telemetry(runtime, sample).await?;
        } else if self.next_plan_retry.is_some_and(|at| Instant::now() >= at) {
            self.last_plan_signature = None;
            self.next_plan_retry = None;
            self.plan_current_candidates(runtime).await?;
        }
        Ok(())
    }

    async fn on_shutdown(&mut self, _runtime: &mut ModeRuntime) -> Result<()> {
        if let Some(recovery) = &mut self.recovery_runtime {
            recovery.cancel_advisor();
        }
        self.active
            .store(false, std::sync::atomic::Ordering::Release);
        self.stop_planning();
        self.pending_telemetry = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AllowedAction;
    use safe_llm_adapter::AdapterRegistry;
    use serde_json::json;

    const PROFILE_FIXTURE: &str = include_str!("../testdata/static_nominal_profile.json");
    const TELEMETRY_FIXTURE: &str = include_str!("../testdata/static_nominal_telemetry.jsonl");

    fn configured_mode() -> AnomalyRecoveryMode {
        let config = serde_json::from_str(PROFILE_FIXTURE).expect("fixture should parse");
        let mut mode = AnomalyRecoveryMode::new(AdapterRegistry::with_builtin_adapters());
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

    #[tokio::test]
    async fn shutdown_handler_serializes_execution_and_rejects_obsolete_intents() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Executor(AtomicUsize);
        #[async_trait]
        impl crate::actions::ShutdownExecutor for Executor {
            async fn shutdown(&self, command: &[String]) -> Result<()> {
                assert_eq!(command, ["/opt/custom-shutdown", "--poweroff"]);
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
        for invalidation in [
            "none",
            "inactive",
            "generation",
            "telemetry",
            "board",
            "pending",
            "cleared",
            "reconfigure",
        ] {
            let mut mode = AnomalyRecoveryMode::new(AdapterRegistry::with_builtin_adapters());
            let mut config: AnomalyRecoveryModeConfig =
                serde_json::from_str(include_str!("../testdata/shutdown_profile.json")).unwrap();
            config.shutdown_command = vec!["/opt/custom-shutdown".into(), "--poweroff".into()];
            mode.set_config(config).unwrap();
            mode.active.store(true, Ordering::Release);
            mode.planning_generation.store(1, Ordering::Release);
            let frame = sample(1, json!({"telemetry": {"temperature_c": 70.0}}));
            mode.evaluate_static_profile(&frame);
            mode.evaluate_static_profile(&frame);
            mode.live_context.update_telemetry(frame.clone(), 8);
            mode.live_context.update_board(Default::default());
            mode.shutdown_intent = Some(crate::actions::tests::intent());
            match invalidation {
                "none" => {}
                "inactive" => mode.active.store(false, Ordering::Release),
                "generation" => {
                    mode.planning_generation.fetch_add(1, Ordering::AcqRel);
                }
                "telemetry" => mode.live_context.update_telemetry(frame.clone(), 8),
                "board" => mode.live_context.update_board(Default::default()),
                "pending" => mode.pending_telemetry = Some(frame),
                "cleared" => mode.current_candidates.clear(),
                "reconfigure" => mode.set_config(mode.config.clone()).unwrap(),
                _ => unreachable!(),
            }
            let executor = Arc::new(Executor(AtomicUsize::new(0)));
            mode.shutdown_controller =
                crate::actions::ShutdownController::with_executor(executor.clone());
            let directory = tempfile::tempdir().unwrap();
            mode.execute_pending_shutdown(directory.path()).await;
            mode.execute_pending_shutdown(directory.path()).await;
            assert_eq!(
                executor.0.load(Ordering::SeqCst),
                usize::from(invalidation == "none"),
                "{invalidation}"
            );
        }
    }

    #[tokio::test]
    async fn telemetry_is_coalesced_without_invalidating_an_in_flight_plan() {
        let mut mode = configured_mode();
        let frozen = sample(1, json!({"telemetry":{"temperature_c":50.0}}));
        mode.latest_telemetry = Some(frozen.clone());
        mode.live_context.update_telemetry(frozen, 8);
        let generation = mode
            .planning_generation
            .load(std::sync::atomic::Ordering::Acquire);
        mode.planning_task = Some(tokio::spawn(async {
            std::future::pending::<Result<Option<crate::actions::ShutdownIntent>>>().await
        }));

        assert!(mode.defer_telemetry_while_planning(sample(
            2,
            json!({"telemetry":{"temperature_c":51.0}})
        )));
        assert!(mode.defer_telemetry_while_planning(sample(
            3,
            json!({"telemetry":{"temperature_c":52.0}})
        )));

        assert_eq!(mode.latest_telemetry.as_ref().unwrap().ts_mono, 1);
        assert_eq!(mode.live_context.snapshot().telemetry.unwrap().ts_mono, 1);
        assert_eq!(mode.pending_telemetry.as_ref().unwrap().ts_mono, 3);
        assert_eq!(
            mode.planning_generation
                .load(std::sync::atomic::Ordering::Acquire),
            generation
        );
        mode.stop_planning();
    }

    #[tokio::test]
    async fn finished_plan_can_be_reaped_before_processing_pending_telemetry() {
        let mut mode = configured_mode();
        mode.pending_telemetry = Some(sample(2, json!({"telemetry":{}})));
        mode.planning_cancel = Some(safe_sim::CancellationToken::new());
        mode.planning_task = Some(tokio::spawn(async { Ok(None) }));
        tokio::task::yield_now().await;

        mode.reap_finished_plan().await;

        assert!(!mode.planning_in_flight());
        assert!(mode.planning_task.is_none());
        assert!(mode.planning_cancel.is_none());
        assert_eq!(mode.pending_telemetry.as_ref().unwrap().ts_mono, 2);
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
    fn decision_trace_text_is_single_line_and_bounded() {
        let input = format!("line one\nline two\t{}", "x".repeat(1_000));
        let trace = trace_text(&input, 1_000);
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
}
