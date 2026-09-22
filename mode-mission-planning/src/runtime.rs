use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use safe::mode_runtime::{ModeHandler, ModeRuntime};
use safe::protocol::{AutonomyModeBoardState, CommandEnvelope, TimedCommand};
use safe::telemetry_frame::TelemetryFrame;
use tracing::{info, warn};

use crate::config::MissionPlanningConfig;
use crate::planning::build_plan;
use crate::simulation::{extract_planning_samples, run_schedule, validate_candidate_schedule};

#[derive(Default)]
pub(crate) struct MissionPlanningMode {
    config: MissionPlanningConfig,
    latest_telemetry: Option<TelemetryFrame>,
    latest_board: AutonomyModeBoardState,
    plan_on_next_telemetry: bool,
    last_plan_completed: Option<Instant>,
    power_saving: bool,
}

impl MissionPlanningMode {
    async fn handle_activation(&mut self, runtime: &mut ModeRuntime) -> Result<()> {
        if self.replan_interval_active() {
            self.emit(runtime, TimedCommand::NOOP).await?;
            return Ok(());
        }
        let Some(telemetry) = self.latest_telemetry.clone() else {
            self.plan_on_next_telemetry = true;
            return Ok(());
        };
        self.run_once(runtime, telemetry).await
    }

    fn replan_interval_active(&self) -> bool {
        self.last_plan_completed.is_some_and(|completed| {
            completed.elapsed() < Duration::from_secs(self.config.min_replan_interval_secs)
        })
    }

    async fn run_once(
        &mut self,
        runtime: &mut ModeRuntime,
        telemetry: TelemetryFrame,
    ) -> Result<()> {
        self.plan_on_next_telemetry = false;
        if let Err(error) = self.plan_and_emit(runtime, &telemetry).await {
            if telemetry_is_not_ready(&error) {
                self.plan_on_next_telemetry = true;
                info!(reason = %error, "mission planning waiting for simulation-ready telemetry");
                return Ok(());
            }
            let message = format!("mission planning failed without emitting commands: {error:#}");
            warn!(reason = %message);
            runtime.fault(message).await?;
        } else {
            self.last_plan_completed = Some(Instant::now());
        }
        Ok(())
    }

    async fn plan_and_emit(
        &mut self,
        runtime: &mut ModeRuntime,
        telemetry: &TelemetryFrame,
    ) -> Result<()> {
        info!(
            simulation_count = 1,
            planning_horizon_secs = self.config.planning_horizon_secs,
            "mission planning running baseline simulation"
        );
        let current_gps_time = telemetry_number(
            telemetry,
            &self.config.telemetry_gps_time_pointer,
            "GPS time",
        )?;
        let current_state_of_charge = telemetry_number(
            telemetry,
            &self.config.telemetry_state_of_charge_pointer,
            "state of charge",
        )?;
        if !(0.0..=1.0).contains(&current_state_of_charge) {
            bail!("telemetry state of charge must be in [0, 1]");
        }

        let accepted_commands = accepted_commands(&self.latest_board, current_gps_time);
        let (simulation_start_mjd, baseline_result) =
            run_schedule(&self.config, telemetry, accepted_commands.clone())
                .await
                .context("baseline simulation")?;
        let samples = extract_planning_samples(&self.config, &baseline_result)?;
        // The planning samples retain the needed values; release full EDS frames before validation.
        drop(baseline_result);
        let plan = build_plan(
            &self.config,
            &samples,
            simulation_start_mjd,
            current_gps_time,
            current_state_of_charge,
            self.power_saving,
        )?;

        let candidates = plan
            .commands
            .into_iter()
            .filter(|candidate| {
                !board_contains_equivalent(
                    &self.latest_board,
                    candidate,
                    self.config.command_dedup_tolerance_secs,
                )
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            self.power_saving = plan.current_power_saving;
            self.emit(runtime, TimedCommand::NOOP).await?;
            return Ok(());
        }

        let mut validation_schedule = accepted_commands;
        validation_schedule.extend(candidates.iter().cloned());
        validation_schedule.sort_by(|left, right| {
            execution_time(left, current_gps_time)
                .total_cmp(&execution_time(right, current_gps_time))
        });
        let validation_simulation_count = 1 + self
            .config
            .monte_carlo
            .as_ref()
            .map_or(0, |monte_carlo| monte_carlo.samples);
        info!(
            simulation_count = validation_simulation_count,
            planning_horizon_secs = self.config.planning_horizon_secs,
            "mission planning running candidate validation simulations"
        );
        validate_candidate_schedule(&self.config, telemetry, validation_schedule)
            .await
            .context("candidate-command Monte Carlo validation")?;

        let command_count = candidates.len();
        for command in candidates {
            self.emit(runtime, command).await?;
        }
        self.power_saving = plan.current_power_saving;
        info!(command_count, "emitted validated mission plan");
        Ok(())
    }

    async fn emit(&self, runtime: &mut ModeRuntime, cmd: TimedCommand) -> Result<()> {
        runtime
            .command(CommandEnvelope {
                from: runtime.mode_id(),
                cmd,
            })
            .await
    }
}

#[async_trait]
impl ModeHandler<MissionPlanningConfig> for MissionPlanningMode {
    fn set_config(&mut self, config: MissionPlanningConfig) -> Result<()> {
        config.validate()?;
        self.config = config;
        Ok(())
    }

    async fn on_activate(&mut self, runtime: &mut ModeRuntime) -> Result<()> {
        self.handle_activation(runtime).await
    }

    async fn on_deactivate(&mut self, _runtime: &mut ModeRuntime) -> Result<()> {
        self.plan_on_next_telemetry = false;
        Ok(())
    }

    async fn on_telemetry(
        &mut self,
        runtime: &mut ModeRuntime,
        telemetry: TelemetryFrame,
    ) -> Result<()> {
        self.latest_telemetry = Some(telemetry.clone());
        if runtime.is_active() && self.plan_on_next_telemetry {
            self.run_once(runtime, telemetry).await?;
        }
        Ok(())
    }

    async fn on_board_snapshot(
        &mut self,
        _runtime: &mut ModeRuntime,
        board: AutonomyModeBoardState,
    ) -> Result<()> {
        self.latest_board = board;
        Ok(())
    }
}

fn telemetry_is_not_ready(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("telemetry has no ")
        || message.contains("telemetry GPS time at JSON Pointer")
        || message.contains("telemetry state of charge at JSON Pointer")
        || message.contains("OTP-2 simulation input is missing derived ")
        || message.contains("OTP-2 simulation input is missing battery voltage")
}

fn telemetry_number(telemetry: &TelemetryFrame, pointer: &str, label: &str) -> Result<f64> {
    let value = telemetry
        .payload
        .pointer(pointer)
        .with_context(|| format!("telemetry has no {label} at JSON Pointer '{pointer}'"))?
        .as_f64()
        .with_context(|| format!("telemetry {label} at JSON Pointer '{pointer}' is not numeric"))?;
    if !value.is_finite() {
        bail!("telemetry {label} must be finite");
    }
    Ok(value)
}

fn accepted_commands(board: &AutonomyModeBoardState, current_gps_time: f64) -> Vec<TimedCommand> {
    let mut commands = board
        .source_of_truth
        .iter()
        .filter_map(|id| {
            board
                .proposals
                .get(id)
                .map(|(_, command, proposal_time)| (command.clone(), *proposal_time, id.0.clone()))
        })
        .filter(|(command, _, _)| !matches!(command, TimedCommand::NOOP))
        .collect::<Vec<_>>();
    commands.sort_by(|left, right| {
        execution_time(&left.0, current_gps_time)
            .total_cmp(&execution_time(&right.0, current_gps_time))
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    commands
        .into_iter()
        .map(|(command, _, _)| command)
        .collect()
}

fn board_contains_equivalent(
    board: &AutonomyModeBoardState,
    candidate: &TimedCommand,
    tolerance_secs: f64,
) -> bool {
    board
        .source_of_truth
        .iter()
        .filter_map(|id| board.proposals.get(id))
        .any(|(_, existing, _)| equivalent(existing, candidate, tolerance_secs))
}

fn equivalent(left: &TimedCommand, right: &TimedCommand, tolerance_secs: f64) -> bool {
    match (left, right) {
        (TimedCommand::NOOP, TimedCommand::NOOP) => true,
        (TimedCommand::Now(left), TimedCommand::Now(right)) => {
            command_value(left) == command_value(right)
        }
        (
            TimedCommand::Scheduled {
                cmd: left,
                gps_time: left_time,
            },
            TimedCommand::Scheduled {
                cmd: right,
                gps_time: right_time,
            },
        ) => {
            command_value(left) == command_value(right)
                && (left_time - right_time).abs() <= tolerance_secs
        }
        _ => false,
    }
}

fn command_value(command: &safe::protocol::Command) -> serde_json::Value {
    serde_json::to_value(command).expect("Command serialization cannot fail")
}

fn execution_time(command: &TimedCommand, current_gps_time: f64) -> f64 {
    match command {
        TimedCommand::Now(_) => current_gps_time,
        TimedCommand::Scheduled { gps_time, .. } => *gps_time,
        TimedCommand::NOOP => f64::INFINITY,
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use safe::protocol::{AutonomyModeId, BoardCmdId, Command};
    use uuid::Uuid;

    use super::*;

    #[test]
    fn duplicate_detection_uses_only_source_of_truth() {
        let id = BoardCmdId("one".into());
        let command = TimedCommand::Scheduled {
            cmd: Command::CaptureImage,
            gps_time: 100.0,
        };
        let mut board = AutonomyModeBoardState::default();
        board.proposals.insert(
            id.clone(),
            (AutonomyModeId(Uuid::nil()), command.clone(), 1),
        );
        assert!(!board_contains_equivalent(&board, &command, 1.0));
        board.source_of_truth.push(id.clone());
        assert!(board_contains_equivalent(&board, &command, 1.0));
    }

    #[test]
    fn completed_plan_activates_replan_interval() {
        let mode = MissionPlanningMode {
            config: MissionPlanningConfig {
                min_replan_interval_secs: 60,
                ..MissionPlanningConfig::default()
            },
            last_plan_completed: Some(Instant::now()),
            ..MissionPlanningMode::default()
        };
        assert!(mode.replan_interval_active());
    }

    #[test]
    fn unrelated_adapter_failure_is_not_transient() {
        let error = anyhow!(
            "simulation input adapter failed (code=Some(1)): Error: invalid telemetry packet"
        );

        assert!(!telemetry_is_not_ready(&error));
    }
}
