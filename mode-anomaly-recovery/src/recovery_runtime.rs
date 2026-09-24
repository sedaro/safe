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
use crate::types::TelemetrySample;

pub(crate) struct RecoveryRuntime {
    pub controller: RecoveryController,
    pub executor: Arc<dyn ShutdownExecutor>,
    noop_sent: bool,
    detail: String,
    last_status: Option<Instant>,
    clock: Option<(f64, Instant)>,
    observed_after: Option<f64>,
    history: VecDeque<Value>,
    advisor_task: Option<tokio::task::JoinHandle<Result<crate::advisor::Assessment>>>,
    last_advisor: Option<Instant>,
    assessed_episode: Option<String>,
    advisor_paused: bool,
    warning_pending: bool,
    service_attempt: Option<Instant>,
    service_error: Option<String>,
    board_context: Value,
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
        }
    }

    pub fn note_board(&mut self, board: &safe::protocol::AutonomyModeBoardState) {
        let mut commands: Vec<_> = board.proposals.iter().collect();
        commands.sort_by(|a, b| a.0.0.cmp(&b.0.0));
        self.board_context = json!({"proposed_count":board.proposals.len(),"approved_count":board.approved.len(),
            "published_count":board.source_of_truth.len(),"sample":commands.into_iter().take(16).collect::<Vec<_>>(),
            "omitted_count":board.proposals.len().saturating_sub(16),"meaning":"command intent, not execution acknowledgement"});
    }

    pub fn activate(&mut self) {
        self.noop_sent = false;
    }

    pub fn cancel_advisor(&mut self) {
        if let Some(task) = self.advisor_task.take() {
            task.abort();
        }
    }

    pub async fn process(
        &mut self,
        runtime: &mut ModeRuntime,
        config: &AnomalyRecoveryModeConfig,
        registry: &AdapterRegistry,
        sample: Option<&TelemetrySample>,
        warning: bool,
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
            self.warning_pending = warning;
            self.history
                .push_back(json!({"observed_at":now,"measurements":self.controller.measurements}));
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
                "inference_requests_paused":held,"advisor_service_paused":self.advisor_paused,"advisor_service_error":self.service_error,
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
        runtime: &ModeRuntime,
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
                Ok(Ok(assessment)) => {
                    info!(assessment = ?assessment, "recovery advisory assessment completed");
                    if let Err(error) = durable_write(
                        runtime.working_directory(),
                        "advisory-assessment.json",
                        &json!({"episode_id":self.assessed_episode,"assessment":assessment}),
                    ) {
                        warn!(%error, "cannot persist advisory assessment");
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
                let evidence = json!({"kind":if post_recovery {"post_recovery"} else {"noncritical_investigation"},
                    "episode":self.controller.episode,"policy":self.controller.config,"recent_measurements":self.history,"command_board":self.board_context});
                self.advisor_task = Some(tokio::spawn(crate::advisor::assess(
                    Arc::from(adapter),
                    config.llm.clone(),
                    evidence,
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
