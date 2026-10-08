use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use safe::mode_runtime::ModeRuntime;
use safe::protocol::{
    AutonomyModeBoardState, AutonomyModeId, BoardCmdId, Command, CommandEnvelope, TimedCommand,
};
use safe_llm_adapter::AdapterRegistry;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::actions::{LinuxShutdown, ShutdownExecutor};
use crate::config::AnomalyRecoveryModeConfig;
use crate::recovery::{Decision, EpisodeKind, RecoveryController, durable_write};
use crate::types::{AnomalyCandidate, TelemetrySample};

pub(crate) struct RecoveryRuntime {
    pub controller: RecoveryController,
    pub executor: Arc<dyn ShutdownExecutor>,
    noop_sent: bool,
    soc_handoff_ready: bool,
    cancellations_pending: HashSet<BoardCmdId>,
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
            soc_handoff_ready: false,
            cancellations_pending: HashSet::new(),
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
        self.soc_handoff_ready = false;
    }

    /// Reconcile durable intent with SAFE's board, including rejected proposals.
    /// Observations may be persisted while inactive, but only active instances emit.
    async fn reconcile_soc(&mut self, runtime: &mut ModeRuntime) -> Result<()> {
        if self.controller.blocked.is_some() || !self.controller.clock_valid {
            return Ok(());
        }
        let Some(episode) = self
            .controller
            .episode
            .as_ref()
            .filter(|e| e.kind == EpisodeKind::SocSunPoint)
        else {
            return Ok(());
        };
        let Some(board) = &self.board else {
            return Ok(());
        };
        let completed = episode.completed_at.is_some();
        let progress = episode.soc.as_ref().expect("SOC progress");
        let gps_time = progress.sun_point_gps_time;
        let mut matching: Vec<_> = board
            .proposals
            .iter()
            .filter(|(_, (from, cmd, _))| {
                matches_sun_point(*from, cmd, runtime.mode_id(), gps_time)
            })
            .map(|(id, _)| id.clone())
            .collect();
        matching.sort_by(|a, b| a.0.cmp(&b.0));
        let command_id = progress
            .command_id
            .clone()
            .or_else(|| matching.first().cloned());
        let status = match &command_id {
            Some(id) if board_rejected(board, id) => "rejected",
            Some(id) if !board.proposals.contains_key(id) => "missing",
            Some(id) if board.source_of_truth.contains(id) => "approved",
            Some(_) => "proposed",
            None if progress.submission_reserved => {
                "submission reserved; awaiting board confirmation"
            }
            None => "ready to submit",
        };
        let confirmed: Vec<_> = progress
            .cancellation_targets
            .iter()
            .filter(|id| board_rejected(board, id))
            .cloned()
            .collect();
        let mut changed = progress.command_id != command_id
            || progress.command_status != status
            || (!progress.submission_reserved && command_id.is_some());
        let progress = self
            .controller
            .episode
            .as_mut()
            .unwrap()
            .soc
            .as_mut()
            .unwrap();
        progress.command_id = command_id.clone();
        progress.command_status = status.into();
        progress.submission_reserved |= command_id.is_some();
        for id in confirmed {
            self.cancellations_pending.remove(&id);
            if !progress.cancelled_commands.contains(&id) {
                progress.cancelled_commands.push(id);
                changed = true;
            }
        }
        progress.cancelled_commands.sort_by(|a, b| a.0.cmp(&b.0));
        if changed {
            self.controller.persist(runtime.working_directory())?;
        }
        if completed || !runtime.is_active() || self.noop_sent || self.soc_handoff_ready {
            return Ok(());
        }
        let mut cancel: Vec<_> = board
            .proposals
            .keys()
            .filter(|id| {
                Some(*id) != command_id.as_ref()
                    && !board_rejected(board, id)
                    && !self.cancellations_pending.contains(*id)
            })
            .cloned()
            .collect();
        cancel.sort_by(|a, b| a.0.cmp(&b.0));
        let progress = self
            .controller
            .episode
            .as_mut()
            .unwrap()
            .soc
            .as_mut()
            .unwrap();
        let mut targets_changed = false;
        for id in &cancel {
            if !progress.cancellation_targets.contains(id) {
                progress.cancellation_targets.push(id.clone());
                targets_changed = true;
            }
        }
        if targets_changed {
            progress.cancellation_targets.sort_by(|a, b| a.0.cmp(&b.0));
            self.controller.persist(runtime.working_directory())?;
        }
        for id in cancel {
            runtime
                .cancel_board(id.clone(), "SOC anomaly recovery supersedes this command")
                .await?;
            self.cancellations_pending.insert(id);
        }
        let progress = self
            .controller
            .episode
            .as_mut()
            .unwrap()
            .soc
            .as_mut()
            .unwrap();
        if !progress.submission_reserved {
            progress.submission_reserved = true;
            progress.command_status = "submission reserved; awaiting board confirmation".into();
            // No command ID is assigned until SAFE returns a board snapshot. Reserve
            // before sending so an uncertain transport/crash cannot produce duplicates.
            self.controller.persist(runtime.working_directory())?;
            runtime
                .command(CommandEnvelope {
                    from: runtime.mode_id(),
                    cmd: TimedCommand::Scheduled {
                        cmd: Command::PointSunYaw,
                        gps_time,
                    },
                })
                .await?;
            info!(
                gps_time,
                "SOC recovery cancelled board commands and scheduled sun-pointing"
            );
        }
        Ok(())
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

        if runtime.is_active() {
            if let Decision::StartSoc(reasons) = self.controller.decision(now) {
                // With the SOC activation gate, a previous nominal NOOP may leave
                // us selected as the fallback. A new confirmed SOC episode must
                // still act without requiring a deactivate/activate round trip.
                self.noop_sent = false;
                self.soc_handoff_ready = false;
                self.cancel_advisor();
                if let Err(error) =
                    self.controller
                        .reserve_soc(runtime.working_directory(), now, reasons)
                {
                    warn!(%error, "cannot persist SOC recovery episode");
                }
            }
        }
        if let Err(error) = self.reconcile_soc(runtime).await {
            warn!(%error, "SOC board reconciliation failed");
            return Ok(());
        }
        let finishing_soc = self
            .controller
            .episode
            .as_ref()
            .is_some_and(|e| e.kind == EpisodeKind::SocSunPoint && e.completed_at.is_none());
        let decision = self.controller.decision(now);
        let (held, detail) = match &decision {
            Decision::Hold(reason) => (true, reason.clone()),
            Decision::StartSoc(reasons) => (true, format!("SOC critical: {}", reasons.join(", "))),
            Decision::Shutdown(reasons) => (true, format!("critical: {}", reasons.join(", "))),
            Decision::Release if finishing_soc => (
                false,
                "SOC recovered; activation-config handoff (sun-point schedule retained)".into(),
            ),
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
        if !held && finishing_soc {
            // The selector can deactivate us before forwarding recovered telemetry.
            // Persist completion even when inactive; do not emit a SOC NOOP.
            if let Err(error) = self.controller.complete(runtime.working_directory(), now) {
                warn!(%error, "cannot persist SOC recovery completion");
                return Ok(());
            }
            self.soc_handoff_ready = true;
        }
        if !held && runtime.is_active() && !self.noop_sent && !self.soc_handoff_ready {
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
        if held && runtime.is_active() && !self.noop_sent && !self.soc_handoff_ready {
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
        if !held && (self.noop_sent || self.soc_handoff_ready) {
            self.run_advisor(runtime, config, registry, instant).await;
        }
        if self
            .last_status
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(1))
        {
            let status = json!({"active":runtime.is_active(),"waiting":held,"detail":self.detail,"noop_sent":self.noop_sent,"soc_handoff_ready":self.soc_handoff_ready,
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
                let episode = self.controller.episode.as_ref().map(|episode| json!({"id":episode.id,"kind":episode.kind,"soc":episode.soc,"reasons":episode.reasons,"attempted_at":episode.attempted_at,
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

fn board_rejected(board: &AutonomyModeBoardState, id: &BoardCmdId) -> bool {
    board
        .rejected
        .get(id)
        .is_some_and(|entries| !entries.is_empty())
}

fn matches_sun_point(
    from: AutonomyModeId,
    cmd: &TimedCommand,
    mode_id: AutonomyModeId,
    expected: f64,
) -> bool {
    from == mode_id
        && matches!(cmd,
        TimedCommand::Scheduled { cmd: Command::PointSunYaw, gps_time }
            if (*gps_time - expected).abs() < 0.001)
}
