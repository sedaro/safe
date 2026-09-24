use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use safe::mode_runtime::ModeRuntime;
use safe::protocol::{CommandEnvelope, TimedCommand};
use safe_llm_adapter::AdapterRegistry;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::actions::{LinuxShutdown, ShutdownExecutor};
use crate::config::AnomalyRecoveryModeConfig;
use crate::recovery::{Decision, RecoveryController, durable_write};
use crate::types::{AnomalyCandidate, TelemetrySample};

pub(crate) struct RecoveryRuntime {
    pub controller: RecoveryController,
    pub executor: Arc<dyn ShutdownExecutor>,
    noop_sent: bool,
    detail: String,
    last_status: Option<Instant>,
    clock: Option<(f64, Instant)>,
    observed_after: Option<f64>,
    history: VecDeque<Value>,
    advisor_task: Option<tokio::task::JoinHandle<Result<crate::advisor::AdvisoryReport>>>,
    last_advisor: Option<Instant>,
    assessed_episode: Option<String>,
    advisor_paused: bool,
    warning_pending: bool,
    service_attempt: Option<Instant>,
    service_error: Option<String>,
    board_context: Value,
    board: Option<safe::protocol::AutonomyModeBoardState>,
    telemetry: Option<TelemetrySample>,
    candidates: Vec<AnomalyCandidate>,
}

impl RecoveryRuntime {
    pub fn new(config: crate::recovery::RecoveryConfig) -> Self {
        Self {
            controller: RecoveryController::new(config),
            executor: Arc::new(LinuxShutdown),
            noop_sent: false,
            detail: "startup check".into(),
            last_status: None,
            clock: None,
            observed_after: None,
            history: VecDeque::new(),
            advisor_task: None,
            last_advisor: None,
            assessed_episode: None,
            advisor_paused: false,
            warning_pending: false,
            service_attempt: None,
            service_error: None,
            board_context: Value::Null,
            board: None,
            telemetry: None,
            candidates: vec![],
        }
    }

    pub fn note_board(&mut self, board: &safe::protocol::AutonomyModeBoardState) {
        let mut commands: Vec<_> = board.proposals.iter().collect();
        commands.sort_by(|a, b| a.0.0.cmp(&b.0.0));
        let context = json!({"proposed_count":board.proposals.len(),"approved_count":board.approved.len(),
            "published_count":board.source_of_truth.len(),"sample":commands.into_iter().take(16).collect::<Vec<_>>(),
            "omitted_count":board.proposals.len().saturating_sub(16),"meaning":"command intent, not execution acknowledgement"});
        let effects = crate::advisory_simulation::board_has_effects(board);
        if self
            .board
            .as_ref()
            .map(crate::advisory_simulation::board_has_effects)
            != Some(effects)
            || (effects && context != self.board_context)
        {
            self.cancel_advisor();
        }
        self.board = Some(board.clone());
        self.board_context = context;
    }

    pub fn note_candidates(&mut self, candidates: &[AnomalyCandidate]) {
        self.warning_pending = !candidates.is_empty();
        self.candidates = candidates.to_vec();
    }

    pub fn activate(&mut self) {
        self.noop_sent = false;
    }

    pub fn cancel_advisor(&mut self) {
        if let Some(task) = self.advisor_task.take() {
            task.abort();
            self.assessed_episode = None;
        }
    }

    pub async fn process(
        &mut self,
        runtime: &mut ModeRuntime,
        config: &AnomalyRecoveryModeConfig,
        registry: &AdapterRegistry,
        sample: Option<&TelemetrySample>,
    ) -> Result<()> {
        let instant = Instant::now();
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
        if let Some((previous, at)) = self.clock {
            if ((now - previous) - instant.duration_since(at).as_secs_f64()).abs()
                > self.controller.config.max_clock_step_secs as f64
            {
                self.controller.clock_valid = false;
            }
        }
        self.clock = Some((now, instant));
        let observed_after = *self.observed_after.get_or_insert(now);
        self.controller.load(runtime.working_directory());
        if let Some(sample) = sample {
            let expected_source = config
                .simulation
                .as_ref()
                .and_then(|sim| sim.initialization.as_ref())
                .map(|init| init.source.as_str())
                .unwrap_or(&self.controller.config.power.source);
            if sample.source.as_deref() == Some(expected_source) {
                self.telemetry = Some(sample.clone());
            }
            self.controller.observe(sample, now);
            // No buffered pre-start readings may qualify startup or recovery.
            for state in &mut self.controller.measurements {
                if state.measured_at.is_some_and(|at| at < observed_after) {
                    state.valid = false;
                    state.violations = 0;
                    state.recovered_samples = 0;
                    state.recovered_since = None;
                }
            }
            let measurements: Vec<_> = std::iter::once(&self.controller.config.power).chain(&self.controller.config.thermals)
                .zip(&self.controller.measurements).map(|(binding, state)| json!({"id":binding.id,"value":state.value,"units":binding.units,"at":state.measured_at,"valid":state.valid})).collect();
            self.history
                .push_back(json!({"observed_at":now,"measurements":measurements}));
            while self.history.len() > config.advisory.history_samples {
                self.history.pop_front();
            }
        }

        let decision = self.controller.decision(now);
        let (held, detail) = match &decision {
            Decision::Hold(reason) => (true, reason.clone()),
            Decision::Shutdown(reasons) => (true, format!("critical: {}", reasons.join(", "))),
            Decision::Release => (false, "nominal/recovered; NOOP handoff".into()),
        };
        if held {
            self.cancel_advisor();
            if !matches!(decision, Decision::Shutdown(_))
                && runtime.is_active()
                && config.advisory.enabled
                && !self.advisor_paused
                && self
                    .service_attempt
                    .is_none_or(|at| at.elapsed() >= Duration::from_secs(30))
            {
                self.service_attempt = Some(instant);
                if !config.advisory.pause_command.is_empty() {
                    if let Err(error) = self.executor.shutdown(&config.advisory.pause_command).await
                    {
                        warn!(%error, "advisor service pause failed");
                        self.service_error = Some(format!("pause failed: {error:#}"));
                    } else {
                        self.advisor_paused = true;
                        self.service_error = None;
                        self.service_attempt = None;
                    }
                } else {
                    self.advisor_paused = true;
                    self.service_attempt = None;
                }
            }
        }
        if detail != self.detail {
            self.detail = detail;
            info!(active = runtime.is_active(), waiting = held, detail = %self.detail, "deterministic recovery decision");
        }
        if !held && runtime.is_active() && !self.noop_sent {
            if let Err(error) = self.controller.complete(runtime.working_directory(), now) {
                warn!(%error, "cannot persist recovery completion");
                return Ok(());
            }
            runtime
                .command(CommandEnvelope {
                    from: runtime.mode_id(),
                    cmd: TimedCommand::NOOP,
                })
                .await?;
            self.noop_sent = true;
            info!("recovery emitted NOOP; yielding through configured activation rules");
        }
        if held && runtime.is_active() && !self.noop_sent {
            if let Decision::Shutdown(reasons) = decision {
                match self
                    .controller
                    .reserve_shutdown(runtime.working_directory(), now, reasons)
                {
                    Ok(()) => {
                        let outcome = self.executor.shutdown(&config.shutdown_command).await;
                        let episode = self.controller.episode.as_mut().expect("reserved episode");
                        episode.shutdown_outcome = match outcome {
                            Ok(()) => "accepted; power-off not acknowledged".into(),
                            Err(error) => format!("failed or unknown: {error:#}"),
                        };
                        info!(episode = %episode.id, outcome = %episode.shutdown_outcome, "deterministic shutdown attempt");
                        if let Err(error) = self.controller.persist(runtime.working_directory()) {
                            warn!(%error, "cannot update shutdown outcome");
                        }
                    }
                    Err(error) => warn!(%error, "shutdown reservation failed; holding"),
                }
            }
        }
        if !held && self.noop_sent {
            self.run_advisor(runtime, config, registry, instant).await;
        }
        if self
            .last_status
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(1))
        {
            let status = json!({"active":runtime.is_active(),"waiting":held,"detail":self.detail,"noop_sent":self.noop_sent,
                "episode":self.controller.episode,"measurements":self.controller.measurements,
                "clock_valid":self.controller.clock_valid,"advisory_enabled":config.advisory.enabled,
                "inference_requests_paused":held,"advisor_service_paused":self.advisor_paused && !config.advisory.pause_command.is_empty(),"advisor_service_error":self.service_error,
                "remaining_secs":self.controller.episode.as_ref().map(|e| (e.resume_after-now).max(0.0))});
            if let Err(error) =
                durable_write(runtime.working_directory(), "recovery-status.json", &status)
            {
                warn!(%error, "cannot write recovery diagnostics");
            }
            self.last_status = Some(instant);
        }
        Ok(())
    }

    async fn run_advisor(
        &mut self,
        runtime: &mut ModeRuntime,
        config: &AnomalyRecoveryModeConfig,
        registry: &AdapterRegistry,
        now: Instant,
    ) {
        if !config.advisory.enabled {
            return;
        }
        if self.advisor_paused {
            if self
                .service_attempt
                .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
            {
                return;
            }
            self.service_attempt = Some(now);
            if !config.advisory.resume_command.is_empty() {
                if let Err(error) = self
                    .executor
                    .shutdown(&config.advisory.resume_command)
                    .await
                {
                    warn!(%error, "advisor service resume failed");
                    self.service_error = Some(format!("resume failed: {error:#}"));
                    return;
                }
            }
            self.advisor_paused = false;
            self.service_attempt = None;
            self.service_error = None;
        }
        if self
            .advisor_task
            .as_ref()
            .is_some_and(|task| task.is_finished())
        {
            let task = self.advisor_task.take().unwrap();
            match task.await {
                Ok(Ok(report)) => {
                    info!(report = ?report, "recovery advisory assessment completed");
                    let completed_runs = report
                        .simulation
                        .runs
                        .iter()
                        .filter(|run| run.result.as_ref().is_some_and(|result| result.success))
                        .count();
                    if completed_runs > 0 {
                        if let Err(error) =
                            runtime.simulation_completed(completed_runs as u64).await
                        {
                            warn!(%error, "cannot report advisory simulation count");
                        }
                    }
                    if let Err(error) = durable_write(
                        runtime.working_directory(),
                        "advisory-assessment.json",
                        &json!({"episode_id":self.assessed_episode,"report":report}),
                    ) {
                        warn!(%error, "cannot persist advisory assessment");
                    }
                    if report.assessment.is_none() {
                        self.assessed_episode = None;
                    }
                }
                result => warn!(?result, "recovery advisory assessment failed"),
            }
        }
        let completed = self
            .controller
            .episode
            .as_ref()
            .filter(|e| e.completed_at.is_some())
            .map(|e| e.id.clone());
        let post_recovery = completed.is_some() && completed != self.assessed_episode;
        if self.advisor_task.is_some()
            || (!post_recovery && !self.warning_pending)
            || self.last_advisor.is_some_and(|at| {
                at.elapsed() < Duration::from_secs(config.advisory.min_interval_secs)
            })
        {
            return;
        }
        self.last_advisor = Some(now);
        match registry.build(&config.llm.adapter) {
            Ok(adapter) => {
                self.assessed_episode = completed;
                let episode = self.controller.episode.as_ref().map(|episode| json!({"id":episode.id,"reasons":episode.reasons,"attempted_at":episode.attempted_at,
                    "resume_after":episode.resume_after,"completed_at":episode.completed_at,"shutdown_outcome":episode.shutdown_outcome}));
                let candidates: Vec<_> = self.candidates.iter().map(|candidate| json!({"id":candidate.rule_id,"path":candidate.path,"observed":candidate.observed,"expectation":candidate.expectation})).collect();
                let evidence = json!({"kind":if post_recovery {"post_recovery"} else {"noncritical_investigation"},
                    "episode":episode,"candidates":candidates,"recent_measurements":self.history,"command_board":self.board_context});
                self.advisor_task = Some(tokio::spawn(crate::advisor::run(
                    Arc::from(adapter),
                    crate::advisor::AdvisoryRequest {
                        config: config.clone(),
                        evidence,
                        telemetry: self.telemetry.clone(),
                        board: self.board.clone(),
                        candidate_ids: self
                            .candidates
                            .iter()
                            .map(|candidate| candidate.rule_id.clone())
                            .collect(),
                        post_recovery,
                    },
                )));
            }
            Err(error) => warn!(%error, "optional recovery advisor unavailable"),
        }
    }
}

impl Drop for RecoveryRuntime {
    fn drop(&mut self) {
        self.cancel_advisor();
    }
}
