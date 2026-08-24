use std::time::{Duration, Instant};

use async_trait::async_trait;
use nalgebra::{Quaternion, UnitQuaternion};
use safe::mode_runtime::{ModeHandler, ModeRuntime};
use safe::protocol::{
    AutonomyModeBoardState, AutonomyModeId, BoardCmdId, Command, CommandEnvelope, TimedCommand,
};
use safe::telemetry_frame::TelemetryFrame;
use safe::utils::{SECONDS_PER_DAY, gps_to_utc_mjd, utc_mjd_to_gps};
use safe_telemetry::model::Telemetry;
use tracing::{debug, info, warn};

use crate::config::ElectronicWarfareModeConfig;
use crate::types::{
    ElectronicWarfareMode, ElectronicWarfarePlan, PlanningOutcome, PointingTarget,
    ScheduledPointing,
};

impl ElectronicWarfareMode {
    fn can_replan_now(&self) -> bool {
        self.last_replan_start
            .map(|last| last.elapsed() >= Duration::from_secs(self.config.min_replan_interval_secs))
            .unwrap_or(true)
    }

    fn board_command_active(&self, id: &BoardCmdId) -> bool {
        self.latest_board_snapshot
            .rejected
            .get(id)
            .is_none_or(Vec::is_empty)
    }

    fn command_matches_target(&self, command: &Command, target: &PointingTarget) -> bool {
        match (command, target) {
            (Command::PointNadir, PointingTarget::Nadir) => true,
            (Command::PointQuaternion { x, y, z, w }, PointingTarget::Quaternion(target)) => {
                let existing = UnitQuaternion::new_normalize(Quaternion::new(*w, *x, *y, *z));
                let dot = existing
                    .quaternion()
                    .coords
                    .dot(&target.quaternion().coords)
                    .abs()
                    .clamp(-1.0, 1.0);
                2.0 * dot.acos() <= self.config.command_dedup_angle_rad
            }
            _ => false,
        }
    }

    fn scheduled_command_matches(
        &self,
        timed_command: &TimedCommand,
        planned: &ScheduledPointing,
    ) -> bool {
        let TimedCommand::Scheduled { cmd, gps_time } = timed_command else {
            return false;
        };
        let Some(time_mjd) = gps_to_utc_mjd(*gps_time) else {
            return false;
        };
        (time_mjd - planned.time_mjd).abs() * SECONDS_PER_DAY <= self.config.command_dedup_time_secs
            && self.command_matches_target(cmd, &planned.target)
    }

    fn is_pointing_command(command: &Command) -> bool {
        matches!(
            command,
            Command::PointNadir
                | Command::PointSunYaw
                | Command::PointThruster
                | Command::PointQuaternion { .. }
        )
    }

    fn selected_target_at(plan: &ElectronicWarfarePlan, time_mjd: f64) -> Option<&PointingTarget> {
        plan.commands
            .iter()
            .take_while(|command| command.time_mjd <= time_mjd)
            .last()
            .map(|command| &command.target)
    }

    fn reconciliation_actions(
        &self,
        mode_id: AutonomyModeId,
        plan: &ElectronicWarfarePlan,
    ) -> (Vec<BoardCmdId>, Vec<ScheduledPointing>) {
        let mut cancel = Vec::new();
        for (id, (from, timed_command, _ts_mono)) in &self.latest_board_snapshot.proposals {
            if !self.board_command_active(id) {
                continue;
            }
            let TimedCommand::Scheduled { cmd, gps_time } = timed_command else {
                continue;
            };
            let Some(time_mjd) = gps_to_utc_mjd(*gps_time) else {
                continue;
            };
            if time_mjd < plan.earliest_command_mjd || time_mjd > plan.horizon_end_mjd {
                continue;
            }
            let is_accepted = self.latest_board_snapshot.source_of_truth.contains(id);
            let is_own = *from == mode_id;
            if !Self::is_pointing_command(cmd) || (!is_accepted && !is_own) {
                continue;
            }
            let keep = if is_accepted {
                Self::selected_target_at(plan, time_mjd)
                    .is_some_and(|target| self.command_matches_target(cmd, target))
            } else {
                plan.commands
                    .iter()
                    .any(|planned| self.scheduled_command_matches(timed_command, planned))
            };
            if !keep {
                cancel.push(id.clone());
            }
        }
        cancel.sort_by(|a, b| a.0.cmp(&b.0));
        cancel.dedup();

        let propose = plan
            .commands
            .iter()
            .filter(|planned| {
                !self.latest_board_snapshot.proposals.iter().any(
                    |(id, (from, timed_command, _ts_mono))| {
                        self.board_command_active(id)
                            && !cancel.contains(id)
                            && (self.latest_board_snapshot.source_of_truth.contains(id)
                                || *from == mode_id)
                            && self.scheduled_command_matches(timed_command, planned)
                    },
                )
            })
            .cloned()
            .collect();
        (cancel, propose)
    }

    async fn emit_plan(
        &self,
        runtime: &mut ModeRuntime,
        plan: &ElectronicWarfarePlan,
    ) -> anyhow::Result<()> {
        let (cancel, propose) = self.reconciliation_actions(runtime.mode_id(), plan);
        for id in &cancel {
            runtime
                .cancel_board(
                    id.clone(),
                    "coorbital-evasion selected pointing schedule supersedes this command",
                )
                .await?;
        }
        for planned in &propose {
            let Some(gps_time) = utc_mjd_to_gps(planned.time_mjd) else {
                anyhow::bail!(
                    "could not convert planned pointing time {} MJD to GPS",
                    planned.time_mjd
                );
            };
            let command = match &planned.target {
                PointingTarget::Nadir => Command::PointNadir,
                PointingTarget::Quaternion(quaternion) => {
                    let q = quaternion.quaternion();
                    Command::PointQuaternion {
                        x: q.i,
                        y: q.j,
                        z: q.k,
                        w: q.w,
                    }
                }
            };
            runtime
                .command(CommandEnvelope {
                    from: runtime.mode_id(),
                    cmd: TimedCommand::Scheduled {
                        cmd: command,
                        gps_time,
                    },
                })
                .await?;
        }
        debug!(
            canceled = cancel.len(),
            proposed = propose.len(),
            "reconciled coorbital-evasion schedule"
        );
        Ok(())
    }

    async fn maybe_plan(
        &mut self,
        runtime: &mut ModeRuntime,
        telemetry: &Telemetry,
    ) -> anyhow::Result<()> {
        if self.config.threat_ids.is_empty() {
            return Ok(());
        }
        let PlanningOutcome::Schedule(plan) = self.build_plan(telemetry).await? else {
            return Ok(());
        };
        if plan.validation.score.exposure_secs > 0.0 {
            warn!(
                validated_exposure_secs = plan.validation.score.exposure_secs,
                modeled_exposure_secs = plan.modeled_score.exposure_secs,
                exposed_threat_ids = ?plan.validation.exposed_threat_ids,
                first_exposed_mjd = plan.validation.first_exposed_mjd,
                last_exposed_mjd = plan.validation.last_exposed_mjd,
                "selected coorbital-evasion schedule remains exposed in finite-dynamics validation; emitting best effort"
            );
        }
        self.emit_plan(runtime, &plan).await?;
        info!(
            pointing_commands = plan.commands.len(),
            modeled_exposure_secs = plan.modeled_score.exposure_secs,
            validated_exposure_secs = plan.validation.score.exposure_secs,
            "coorbital-evasion pointing plan evaluated"
        );
        Ok(())
    }

    fn warn_missing_config_once(&mut self) {
        if self.warned_missing_config {
            return;
        }
        if self.config.eds_path.as_os_str().is_empty() {
            warn!("ElectronicWarfare mode_config.eds_path is not configured; simulation disabled");
        }
        if self.config.field_of_view_id.is_empty() {
            warn!("ElectronicWarfare mode_config.field_of_view_id is empty");
        }
        if self.config.threat_ids.is_empty() {
            warn!(
                "ElectronicWarfare mode_config.threat_ids is empty; no threats will be evaluated"
            );
        }
        self.warned_missing_config = true;
    }

    async fn replan_if_ready(&mut self, runtime: &mut ModeRuntime, telemetry: &Telemetry) {
        if !self.can_replan_now() {
            return;
        }
        self.last_replan_start = Some(Instant::now());
        if let Err(error) = self.maybe_plan(runtime, telemetry).await {
            warn!("coorbital-evasion planning failed: {error:#}");
        }
    }
}

#[async_trait]
impl ModeHandler<ElectronicWarfareModeConfig> for ElectronicWarfareMode {
    fn set_config(&mut self, config: ElectronicWarfareModeConfig) -> anyhow::Result<()> {
        self.config = config;
        self.latest_board_snapshot = AutonomyModeBoardState::default();
        self.has_board_snapshot = false;
        self.last_replan_start = None;
        self.warned_missing_config = false;
        Ok(())
    }

    async fn on_activate(&mut self, runtime: &mut ModeRuntime) -> anyhow::Result<()> {
        self.warn_missing_config_once();
        if !self.has_board_snapshot {
            info!("coorbital-evasion mode waiting for initial board snapshot");
            return Ok(());
        }
        if let Some(telemetry) = self.latest_telemetry.clone() {
            self.replan_if_ready(runtime, &telemetry).await;
        }
        Ok(())
    }

    async fn on_telemetry(
        &mut self,
        runtime: &mut ModeRuntime,
        frame: TelemetryFrame,
    ) -> anyhow::Result<()> {
        let telemetry = match frame.decode_payload::<Telemetry>() {
            Ok(telemetry) => telemetry,
            Err(error) => {
                warn!("coorbital-evasion received incompatible telemetry: {error}");
                return Ok(());
            }
        };
        self.latest_telemetry = Some(telemetry.clone());
        if runtime.is_active() && self.has_board_snapshot {
            self.warn_missing_config_once();
            self.replan_if_ready(runtime, &telemetry).await;
        }
        Ok(())
    }

    async fn on_board_snapshot(
        &mut self,
        runtime: &mut ModeRuntime,
        board: AutonomyModeBoardState,
    ) -> anyhow::Result<()> {
        let first_snapshot = !self.has_board_snapshot;
        self.has_board_snapshot = true;
        self.latest_board_snapshot = board;
        if first_snapshot
            && runtime.is_active()
            && let Some(telemetry) = self.latest_telemetry.clone()
        {
            self.replan_if_ready(runtime, &telemetry).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use safe::protocol::{AutonomyModeBoardState, BoardCmdId};
    use safe::utils::utc_mjd_to_gps;
    use uuid::Uuid;

    use super::*;
    use crate::types::{ScheduleScore, ValidationReport};

    fn plan(commands: Vec<ScheduledPointing>) -> ElectronicWarfarePlan {
        ElectronicWarfarePlan {
            earliest_command_mjd: 60_000.0,
            horizon_end_mjd: 60_001.0,
            commands,
            modeled_score: ScheduleScore::default(),
            validation: ValidationReport::default(),
        }
    }

    fn add_board_command(
        board: &mut AutonomyModeBoardState,
        id: &str,
        from: AutonomyModeId,
        command: Command,
        time_mjd: f64,
        accepted: bool,
    ) {
        let id = BoardCmdId(id.to_string());
        board.proposals.insert(
            id.clone(),
            (
                from,
                TimedCommand::Scheduled {
                    cmd: command,
                    gps_time: utc_mjd_to_gps(time_mjd).unwrap(),
                },
                0,
            ),
        );
        if accepted {
            board.source_of_truth.push(id);
        }
    }

    #[test]
    fn quaternion_equivalence_is_sign_invariant() {
        let mode = ElectronicWarfareMode::new();
        let target = PointingTarget::Quaternion(UnitQuaternion::identity());
        assert!(mode.command_matches_target(
            &Command::PointQuaternion {
                x: -0.0,
                y: -0.0,
                z: -0.0,
                w: -1.0,
            },
            &target
        ));
    }

    #[test]
    fn reconciliation_cancels_conflicts_and_obsolete_own_proposals_only() {
        let own = AutonomyModeId(Uuid::nil());
        let other = AutonomyModeId(Uuid::new_v4());
        let mut mode = ElectronicWarfareMode::new();
        let mut board = AutonomyModeBoardState::default();
        add_board_command(
            &mut board,
            "before",
            other,
            Command::PointSunYaw,
            59_999.9,
            true,
        );
        add_board_command(
            &mut board,
            "conflict",
            other,
            Command::PointSunYaw,
            60_000.2,
            true,
        );
        add_board_command(
            &mut board,
            "obsolete",
            own,
            Command::PointNadir,
            60_000.3,
            false,
        );
        add_board_command(
            &mut board,
            "unrelated",
            other,
            Command::CaptureImage,
            60_000.2,
            true,
        );
        mode.latest_board_snapshot = board;
        let selected = ScheduledPointing {
            time_mjd: 60_000.1,
            target: PointingTarget::Nadir,
        };
        let (cancel, propose) = mode.reconciliation_actions(own, &plan(vec![selected.clone()]));
        assert_eq!(
            cancel,
            vec![
                BoardCmdId("conflict".to_string()),
                BoardCmdId("obsolete".to_string())
            ]
        );
        assert_eq!(propose, vec![selected]);
    }

    #[test]
    fn reconciliation_does_not_repropose_equivalent_nadir() {
        let own = AutonomyModeId(Uuid::nil());
        let mut mode = ElectronicWarfareMode::new();
        let mut board = AutonomyModeBoardState::default();
        add_board_command(
            &mut board,
            "nadir",
            own,
            Command::PointNadir,
            60_000.1,
            true,
        );
        mode.latest_board_snapshot = board;
        let (cancel, propose) = mode.reconciliation_actions(
            own,
            &plan(vec![ScheduledPointing {
                time_mjd: 60_000.1,
                target: PointingTarget::Nadir,
            }]),
        );
        assert!(cancel.is_empty());
        assert!(propose.is_empty());
    }

    #[test]
    fn reconciliation_preserves_redundant_accepted_target() {
        let own = AutonomyModeId(Uuid::nil());
        let other = AutonomyModeId(Uuid::new_v4());
        let mut mode = ElectronicWarfareMode::new();
        let mut board = AutonomyModeBoardState::default();
        add_board_command(
            &mut board,
            "later-nadir",
            other,
            Command::PointNadir,
            60_000.2,
            true,
        );
        mode.latest_board_snapshot = board;
        let (cancel, _) = mode.reconciliation_actions(
            own,
            &plan(vec![ScheduledPointing {
                time_mjd: 60_000.1,
                target: PointingTarget::Nadir,
            }]),
        );
        assert!(cancel.is_empty());
    }

    #[test]
    fn unrelated_pending_proposal_does_not_suppress_emission() {
        let own = AutonomyModeId(Uuid::nil());
        let other = AutonomyModeId(Uuid::new_v4());
        let mut mode = ElectronicWarfareMode::new();
        let mut board = AutonomyModeBoardState::default();
        add_board_command(
            &mut board,
            "pending",
            other,
            Command::PointNadir,
            60_000.1,
            false,
        );
        mode.latest_board_snapshot = board;
        let selected = ScheduledPointing {
            time_mjd: 60_000.1,
            target: PointingTarget::Nadir,
        };

        let (_, propose) = mode.reconciliation_actions(own, &plan(vec![selected.clone()]));

        assert_eq!(propose, vec![selected]);
    }
}
