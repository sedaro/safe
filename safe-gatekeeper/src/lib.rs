use std::collections::HashSet;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use safe::{
    protocol::{BoardCmdId, BoardState, TimedCommand},
    telemetry_frame::TelemetryFrame,
};
use safe_sim::{EdsPatch, SedaroSimulator, SimulationResult};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub mod gatekeeper_types;
mod monte_carlo;

use crate::gatekeeper_types::{
    CheckAggregation, ComparisonOp, FieldCheck, GatekeeperConfig, GatekeeperInput,
    GatekeeperOutput, SimulationInputRequest, SimulationInputResponse,
};

enum CheckOutcome {
    Passed(String),
    Failed(String),
}

fn meets_minimum_pass_fraction(passed: usize, samples: usize, minimum: f64) -> bool {
    passed as f64 / samples as f64 + f64::EPSILON >= minimum
}

impl CheckAggregation {
    fn evaluate_over(&self, values: &[f64]) -> anyhow::Result<f64> {
        if values.is_empty() {
            anyhow::bail!("no values available to aggregate");
        }

        Ok(match self {
            CheckAggregation::Last => *values.last().unwrap(),
            CheckAggregation::Min => values.iter().copied().fold(f64::INFINITY, f64::min),
            CheckAggregation::Max => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            CheckAggregation::Mean => values.iter().sum::<f64>() / values.len() as f64,
        })
    }
}

impl ComparisonOp {
    fn compare(&self, observed: f64, threshold: f64, tolerance: f64) -> bool {
        match self {
            ComparisonOp::Lt => observed < threshold,
            ComparisonOp::Lte => observed <= threshold,
            ComparisonOp::Gt => observed > threshold,
            ComparisonOp::Gte => observed >= threshold,
            ComparisonOp::Eq => (observed - threshold).abs() <= tolerance,
            ComparisonOp::Ne => (observed - threshold).abs() > tolerance,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            ComparisonOp::Lt => "<",
            ComparisonOp::Lte => "<=",
            ComparisonOp::Gt => ">",
            ComparisonOp::Gte => ">=",
            ComparisonOp::Eq => "==",
            ComparisonOp::Ne => "!=",
        }
    }
}

pub struct Gatekeeper {
    config: GatekeeperConfig,
    simulator: SedaroSimulator,
    latest_telemetry: Option<TelemetryFrame>,
}

impl Gatekeeper {
    pub fn new(config: GatekeeperConfig) -> Self {
        Self {
            simulator: SedaroSimulator::new(&config.eds_path),
            config,
            latest_telemetry: None,
        }
    }

    fn commands_for_simulation(
        board: &BoardState,
        candidate_command_ids: &[BoardCmdId],
        current_gps_time: Option<f64>,
    ) -> anyhow::Result<Vec<TimedCommand>> {
        let mut seen = HashSet::new();
        let mut commands = board
            .source_of_truth
            .iter()
            .chain(candidate_command_ids)
            .filter(|id| seen.insert((*id).clone()))
            .map(|id| {
                board
                    .proposals
                    .get(id)
                    .map(|(_from, command, proposal_time)| {
                        (command.clone(), *proposal_time, id.0.clone())
                    })
                    .ok_or_else(|| anyhow::anyhow!("board has no command for id '{}'", id.0))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        commands.retain(|(command, _, _)| !matches!(command, TimedCommand::NOOP));
        let mut commands = commands
            .into_iter()
            .map(|(command, proposal_time, id)| {
                let execution_time = match &command {
                    TimedCommand::Now(_) => current_gps_time.context(
                        "telemetry_gps_time_pointer must be configured when evaluating an immediate command",
                    )?,
                    TimedCommand::Scheduled { gps_time, .. } if gps_time.is_finite() => *gps_time,
                    TimedCommand::Scheduled { .. } => {
                        anyhow::bail!("command '{}' has a non-finite scheduled GPS time", id)
                    }
                    TimedCommand::NOOP => unreachable!("no-ops were removed above"),
                };
                Ok((command, execution_time, proposal_time, id))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        commands.sort_by(|left, right| {
            left.1
                .total_cmp(&right.1)
                .then_with(|| left.2.cmp(&right.2))
                .then_with(|| left.3.cmp(&right.3))
        });

        Ok(commands
            .into_iter()
            .map(|(command, _, _, _)| command)
            .collect())
    }

    fn telemetry_gps_time(&self, telemetry: &TelemetryFrame) -> anyhow::Result<Option<f64>> {
        let Some(pointer) = self.config.telemetry_gps_time_pointer.as_deref() else {
            return Ok(None);
        };
        if !pointer.is_empty() && !pointer.starts_with('/') {
            anyhow::bail!("telemetry_gps_time_pointer must be empty or start with '/'");
        }
        let value = telemetry.payload.pointer(pointer).with_context(|| {
            format!("telemetry payload has no GPS time at JSON Pointer '{pointer}'")
        })?;
        let gps_time = value.as_f64().with_context(|| {
            format!("telemetry GPS time at JSON Pointer '{pointer}' is not numeric")
        })?;
        if !gps_time.is_finite() {
            anyhow::bail!(
                "telemetry GPS time at JSON Pointer '{}' is not finite",
                pointer
            );
        }
        Ok(Some(gps_time))
    }

    fn telemetry_gps_time_for_commands(
        &self,
        telemetry: &TelemetryFrame,
        board: &BoardState,
        candidate_command_ids: &[BoardCmdId],
    ) -> anyhow::Result<Option<f64>> {
        let has_immediate_command = board
            .source_of_truth
            .iter()
            .chain(candidate_command_ids)
            .filter_map(|id| board.proposals.get(id))
            .any(|(_from, command, _proposal_time)| matches!(command, TimedCommand::Now(_)));

        if has_immediate_command {
            self.telemetry_gps_time(telemetry)
        } else {
            Ok(None)
        }
    }

    /// Runs the configured mission adapter once to materialize the EDS epoch
    /// and patches for the latest telemetry and requested command batch.
    async fn build_simulation_input(
        &self,
        request: &SimulationInputRequest,
    ) -> anyhow::Result<SimulationInputResponse> {
        if self.config.input_adapter_timeout_secs == Some(0) {
            anyhow::bail!("gatekeeper input_adapter_timeout_secs must be greater than zero");
        }
        let (executable, args) = self
            .config
            .input_adapter_command
            .split_first()
            .context("gatekeeper input_adapter_command is not configured")?;

        let mut child = Command::new(executable)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to start simulation input adapter '{executable}'"))?;

        let mut stdin = child
            .stdin
            .take()
            .context("simulation input adapter has no stdin")?;
        let encoded = serde_json::to_vec(request)?;
        stdin.write_all(&encoded).await?;
        stdin.write_all(b"\n").await?;
        drop(stdin);

        let wait = child.wait_with_output();
        let output = if let Some(timeout_secs) = self.config.input_adapter_timeout_secs {
            tokio::time::timeout(Duration::from_secs(timeout_secs), wait)
                .await
                .with_context(|| {
                    format!("simulation input adapter timed out after {timeout_secs} seconds")
                })?
        } else {
            wait.await
        }
        .context("failed while waiting for simulation input adapter")?;
        if !output.status.success() {
            anyhow::bail!(
                "simulation input adapter failed (code={:?}): {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        serde_json::from_slice(&output.stdout).with_context(|| {
            format!(
                "simulation input adapter returned invalid JSON: {}",
                String::from_utf8_lossy(&output.stdout).trim()
            )
        })
    }

    async fn run_simulation(
        &self,
        start_time_mjd: f64,
        patches: Vec<EdsPatch>,
    ) -> anyhow::Result<SimulationResult> {
        if self.config.simulation_timeout_secs == Some(0) {
            anyhow::bail!("gatekeeper simulation_timeout_secs must be greater than zero");
        }
        let mut simulator = self
            .simulator
            .clone()
            .at_epoch(start_time_mjd)
            .patch_multi(patches);
        if let Some(timeout_secs) = self.config.simulation_timeout_secs {
            simulator = simulator.timeout(Duration::from_secs(timeout_secs));
        }

        tracing::info!(
            sim_duration_days = self.config.sim_duration_days,
            "Gatekeeper running simulation"
        );
        let result = simulator
            .run_collect(self.config.sim_duration_days)
            .await
            .context("gatekeeper simulation execution or output collection failed")?;
        tracing::info!("Gatekeeper simulation finished");
        Ok(result)
    }

    fn values_for_check(&self, result: &SimulationResult, check: &FieldCheck) -> Vec<f64> {
        result.numeric_field_values(&check.target_file, &check.field)
    }

    fn evaluate(&self, result: &SimulationResult) -> anyhow::Result<CheckOutcome> {
        if !result.success {
            anyhow::bail!(
                "Simulation failed (code={:?}): {}",
                result.exit_code,
                result.stderr
            );
        }

        if self.config.field_checks.is_empty() {
            return Ok(CheckOutcome::Passed(format!(
                "Simulation OK (code={:?}); no field checks configured",
                result.exit_code
            )));
        }

        let mut passed_checks = Vec::new();
        for check in &self.config.field_checks {
            let values = self.values_for_check(result, check);
            if values.is_empty() {
                anyhow::bail!(
                    "Missing values for check: file='{}' field='{}'",
                    check.target_file,
                    check.field
                );
            }

            let observed = check.aggregation.evaluate_over(&values)?;
            if !check.op.compare(observed, check.threshold, check.tolerance) {
                return Ok(CheckOutcome::Failed(format!(
                    "Constraint violated: {}:{} {:?} must be {} {} (observed={:.6})",
                    check.target_file,
                    check.field,
                    check.aggregation,
                    check.op.as_str(),
                    check.threshold,
                    observed
                )));
            }

            passed_checks.push(format!(
                "{}:{} ({:?}) {:.6} {} {:.6}",
                check.target_file,
                check.field,
                check.aggregation,
                observed,
                check.op.as_str(),
                check.threshold
            ));
        }

        Ok(CheckOutcome::Passed(format!(
            "Simulation OK (code={:?}); checks passed [{}]",
            result.exit_code,
            passed_checks.join("; ")
        )))
    }

    /// Runs the exact adapter state first, then evaluates randomized scalar
    /// perturbations independently of the required nominal result.
    async fn run_analysis(&self, input: SimulationInputResponse) -> anyhow::Result<String> {
        if !self.config.sim_duration_days.is_finite() || self.config.sim_duration_days <= 0.0 {
            anyhow::bail!(
                "gatekeeper sim_duration_days must be finite and > 0, got {}",
                self.config.sim_duration_days
            );
        }
        if !input.start_time_mjd.is_finite() {
            anyhow::bail!("simulation input adapter returned a non-finite start_time_mjd");
        }
        for check in &self.config.field_checks {
            if !check.tolerance.is_finite() || check.tolerance < 0.0 {
                anyhow::bail!(
                    "field check tolerance must be finite and >= 0 for file='{}' field='{}'",
                    check.target_file,
                    check.field
                );
            }
        }
        monte_carlo::validate_baseline_patches(&input.patches)?;
        // Generate cases before launching EDS so invalid distributions, patch
        // targets, or bounds fail without spending time on the nominal run.
        let monte_carlo_cases = self
            .config
            .monte_carlo
            .as_ref()
            .map(|config| monte_carlo::generate_cases(config, &input.patches))
            .transpose()?;

        tracing::info!(
            analysis_case = "nominal",
            "Gatekeeper running nominal simulation"
        );
        let nominal_result = self
            .run_simulation(input.start_time_mjd, input.patches.clone())
            .await
            .context("nominal simulation failed")?;
        let nominal_details = match self.evaluate(&nominal_result)? {
            CheckOutcome::Passed(details) => details,
            CheckOutcome::Failed(reason) => anyhow::bail!("Nominal simulation rejected: {reason}"),
        };

        let Some(config) = &self.config.monte_carlo else {
            return Ok(nominal_details);
        };
        let cases = monte_carlo_cases.context("Monte Carlo cases were not generated")?;

        let mut passed = 0usize;
        let mut failed_cases = Vec::new();
        for case in cases {
            tracing::info!(
                analysis_case = %case.id,
                case_seed = case.seed,
                sampled_values = ?case.values,
                "Gatekeeper running Monte Carlo simulation"
            );
            let result = self
                .run_simulation(input.start_time_mjd, case.patches)
                .await
                .with_context(|| format!("Monte Carlo case '{}' failed", case.id))?;
            match self
                .evaluate(&result)
                .with_context(|| format!("Monte Carlo case '{}' could not be evaluated", case.id))?
            {
                CheckOutcome::Passed(_) => passed += 1,
                CheckOutcome::Failed(reason) => failed_cases.push(format!("{}: {reason}", case.id)),
            }
        }

        let pass_fraction = passed as f64 / config.samples as f64;
        tracing::info!(
            samples = config.samples,
            passed,
            failed = config.samples - passed,
            pass_fraction,
            minimum_pass_fraction = config.minimum_pass_fraction,
            seed = config.seed,
            "Gatekeeper completed Monte Carlo analysis"
        );
        if !meets_minimum_pass_fraction(passed, config.samples, config.minimum_pass_fraction) {
            let examples = failed_cases
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::bail!(
                "Monte Carlo rejected: {passed}/{} cases passed ({pass_fraction:.3}, required {:.3}); failures [{}]",
                config.samples,
                config.minimum_pass_fraction,
                examples
            );
        }

        Ok(format!(
            "{nominal_details}; Monte Carlo passed {passed}/{} cases ({pass_fraction:.3}, required {:.3}, seed={})",
            config.samples, config.minimum_pass_fraction, config.seed
        ))
    }

    pub async fn start(
        mut self: Box<Self>,
        mut rx: tokio::sync::mpsc::Receiver<GatekeeperInput>,
        tx: tokio::sync::mpsc::Sender<GatekeeperOutput>,
    ) {
        tracing::info!("Gatekeeper started");
        while let Some(msg) = rx.recv().await {
            match msg {
                // Telemetry interpretation belongs to the input adapter. The
                // gatekeeper only retains the latest opaque snapshot.
                GatekeeperInput::Telemetry(frame) => self.latest_telemetry = Some(frame),
                GatekeeperInput::EvaluateBatch {
                    request_id,
                    board,
                    candidate_command_ids,
                } => {
                    let Some(telemetry) = self.latest_telemetry.clone() else {
                        let _ = tx
                            .send(GatekeeperOutput::Reject {
                                request_id,
                                reason: "No telemetry available yet".to_string(),
                            })
                            .await;
                        continue;
                    };

                    let request: anyhow::Result<SimulationInputRequest> = (|| {
                        let current_gps_time = self.telemetry_gps_time_for_commands(
                            &telemetry,
                            &board,
                            &candidate_command_ids,
                        )?;
                        Ok(SimulationInputRequest {
                            telemetry,
                            commands: Self::commands_for_simulation(
                                &board,
                                &candidate_command_ids,
                                current_gps_time,
                            )?,
                            config: self.config.input_adapter_config.clone(),
                        })
                    })();
                    let out = match request {
                        Ok(request) => match self.build_simulation_input(&request).await {
                            Ok(input) => match self.run_analysis(input).await {
                                Ok(details) => GatekeeperOutput::Approve {
                                    request_id,
                                    details: format!(
                                        "{details}; batch_size={}",
                                        candidate_command_ids.len()
                                    ),
                                },
                                Err(error) => GatekeeperOutput::Reject {
                                    request_id,
                                    reason: format!("Simulation rejected: {error:#}"),
                                },
                            },
                            Err(error) => GatekeeperOutput::Reject {
                                request_id,
                                reason: format!("Simulation error: {error:#}"),
                            },
                        },
                        Err(error) => GatekeeperOutput::Reject {
                            request_id,
                            reason: format!("Simulation input error: {error:#}"),
                        },
                    };

                    let _ = tx.send(out).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use safe::protocol::BoardState;

    #[test]
    fn monte_carlo_acceptance_uses_only_sampled_cases() {
        assert!(meets_minimum_pass_fraction(19, 20, 0.95));
        assert!(!meets_minimum_pass_fraction(18, 20, 0.95));
        assert!(!meets_minimum_pass_fraction(19, 20, 1.0));
    }

    #[test]
    fn equality_comparisons_use_configured_tolerance() {
        assert!(ComparisonOp::Eq.compare(1.001, 1.0, 0.01));
        assert!(!ComparisonOp::Eq.compare(1.001, 1.0, 0.0001));
        assert!(!ComparisonOp::Ne.compare(1.001, 1.0, 0.01));
        assert!(ComparisonOp::Ne.compare(1.001, 1.0, 0.0001));
    }

    #[tokio::test]
    async fn zero_adapter_timeout_is_rejected_before_launch() {
        let gatekeeper = Gatekeeper::new(GatekeeperConfig {
            input_adapter_timeout_secs: Some(0),
            ..GatekeeperConfig::default()
        });
        let request = SimulationInputRequest {
            telemetry: TelemetryFrame::new(serde_json::json!({})),
            commands: Vec::new(),
            config: serde_json::Value::Null,
        };

        let error = gatekeeper
            .build_simulation_input(&request)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("must be greater than zero"));
    }

    #[tokio::test]
    async fn adapter_receives_request_and_returns_materialized_input() {
        let config = GatekeeperConfig {
            input_adapter_command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "read request; printf '%s' '{\"start_time_mjd\":60000.0,\"patches\":[]}'"
                    .to_string(),
            ],
            ..GatekeeperConfig::default()
        };
        let gatekeeper = Gatekeeper::new(config);
        let request = SimulationInputRequest {
            telemetry: TelemetryFrame::new(serde_json::json!({"opaque": true})),
            commands: Vec::new(),
            config: serde_json::Value::Null,
        };

        let response = gatekeeper.build_simulation_input(&request).await.unwrap();
        assert_eq!(response.start_time_mjd, 60000.0);
        assert!(response.patches.is_empty());
    }

    #[tokio::test]
    async fn evaluation_without_telemetry_is_rejected_before_adapter_start() {
        let (in_tx, in_rx) = tokio::sync::mpsc::channel(1);
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(Box::new(Gatekeeper::new(GatekeeperConfig::default())).start(in_rx, out_tx));

        in_tx
            .send(GatekeeperInput::EvaluateBatch {
                request_id: 7,
                board: BoardState::default(),
                candidate_command_ids: Vec::new(),
            })
            .await
            .unwrap();

        let output = out_rx.recv().await.unwrap();
        assert!(matches!(
            output,
            GatekeeperOutput::Reject { request_id: 7, reason }
                if reason == "No telemetry available yet"
        ));
    }

    #[test]
    fn combines_and_orders_commands_by_execution_time() {
        let second = BoardCmdId("second".to_string());
        let third = BoardCmdId("third".to_string());
        let board: BoardState = serde_json::from_value(serde_json::json!({
            "proposals": {
                "first": ["00000000-0000-0000-0000-000000000000", {"Scheduled": {"cmd": "PointNadir", "gps_time": 20.0}}, 1],
                "second": ["00000000-0000-0000-0000-000000000000", {"Scheduled": {"cmd": "CaptureImage", "gps_time": 10.0}}, 2],
                "third": ["00000000-0000-0000-0000-000000000000", {"Now": "PointSunYaw"}, 3]
            },
            "rejected": {},
            "approved": {},
            "source_of_truth": ["first"]
        }))
        .unwrap();

        let commands =
            Gatekeeper::commands_for_simulation(&board, &[second, third], Some(15.0)).unwrap();
        assert!(matches!(
            commands[0],
            TimedCommand::Scheduled {
                cmd: safe::protocol::Command::CaptureImage,
                gps_time: 10.0
            }
        ));
        assert!(matches!(
            commands[1],
            TimedCommand::Now(safe::protocol::Command::PointSunYaw)
        ));
        assert!(matches!(
            commands[2],
            TimedCommand::Scheduled {
                cmd: safe::protocol::Command::PointNadir,
                gps_time: 20.0
            }
        ));
    }

    #[test]
    fn equal_time_commands_retain_proposal_order() {
        let earlier = BoardCmdId("10:earlier".to_string());
        let later = BoardCmdId("2:later".to_string());
        let board: BoardState = serde_json::from_value(serde_json::json!({
            "proposals": {
                "10:earlier": ["00000000-0000-0000-0000-000000000000", {"Scheduled": {"cmd": "PointNadir", "gps_time": 10.0}}, 1],
                "2:later": ["00000000-0000-0000-0000-000000000000", {"Now": "PointSunYaw"}, 2]
            },
            "rejected": {},
            "approved": {},
            "source_of_truth": []
        }))
        .unwrap();

        let commands =
            Gatekeeper::commands_for_simulation(&board, &[later, earlier], Some(10.0)).unwrap();
        assert!(matches!(
            commands[0],
            TimedCommand::Scheduled {
                cmd: safe::protocol::Command::PointNadir,
                ..
            }
        ));
        assert!(matches!(
            commands[1],
            TimedCommand::Now(safe::protocol::Command::PointSunYaw)
        ));
    }

    #[test]
    fn reads_nested_telemetry_gps_time_pointer() {
        let gatekeeper = Gatekeeper::new(GatekeeperConfig {
            telemetry_gps_time_pointer: Some("/spacecraft/clock/gps_time".to_string()),
            ..GatekeeperConfig::default()
        });
        let telemetry = TelemetryFrame::new(serde_json::json!({
            "spacecraft": {"clock": {"gps_time": 1234.5}}
        }));

        assert_eq!(
            gatekeeper.telemetry_gps_time(&telemetry).unwrap(),
            Some(1234.5)
        );
    }

    #[test]
    fn immediate_command_requires_telemetry_gps_time() {
        let id = BoardCmdId("immediate".to_string());
        let board: BoardState = serde_json::from_value(serde_json::json!({
            "proposals": {
                "immediate": ["00000000-0000-0000-0000-000000000000", {"Now": "PointNadir"}, 1]
            },
            "rejected": {},
            "approved": {},
            "source_of_truth": []
        }))
        .unwrap();

        let error = Gatekeeper::commands_for_simulation(&board, &[id], None).unwrap_err();
        assert!(error.to_string().contains("telemetry_gps_time_pointer"));
    }

    #[test]
    fn scheduled_only_commands_do_not_read_telemetry_gps_time() {
        let id = BoardCmdId("scheduled".to_string());
        let board: BoardState = serde_json::from_value(serde_json::json!({
            "proposals": {
                "scheduled": ["00000000-0000-0000-0000-000000000000", {"Scheduled": {"cmd": "PointNadir", "gps_time": 10.0}}, 1]
            },
            "rejected": {},
            "approved": {},
            "source_of_truth": []
        }))
        .unwrap();
        let gatekeeper = Gatekeeper::new(GatekeeperConfig {
            telemetry_gps_time_pointer: Some("/missing".to_string()),
            ..GatekeeperConfig::default()
        });
        let telemetry = TelemetryFrame::new(serde_json::json!({}));

        let gps_time = gatekeeper
            .telemetry_gps_time_for_commands(&telemetry, &board, std::slice::from_ref(&id))
            .unwrap();
        assert_eq!(gps_time, None);
        assert_eq!(
            Gatekeeper::commands_for_simulation(&board, &[id], gps_time)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn duplicate_command_ids_are_included_once() {
        let id = BoardCmdId("duplicate".to_string());
        let board: BoardState = serde_json::from_value(serde_json::json!({
            "proposals": {
                "duplicate": ["00000000-0000-0000-0000-000000000000", {"Scheduled": {"cmd": "PointNadir", "gps_time": 10.0}}, 1]
            },
            "rejected": {},
            "approved": {},
            "source_of_truth": ["duplicate"]
        }))
        .unwrap();

        let commands = Gatekeeper::commands_for_simulation(&board, &[id], None).unwrap();
        assert_eq!(commands.len(), 1);
    }

    #[test]
    fn noops_are_removed_from_the_simulation_schedule() {
        let id = BoardCmdId("noop".to_string());
        let board: BoardState = serde_json::from_value(serde_json::json!({
            "proposals": {
                "noop": ["00000000-0000-0000-0000-000000000000", "NOOP", 1]
            },
            "rejected": {},
            "approved": {},
            "source_of_truth": []
        }))
        .unwrap();

        let commands = Gatekeeper::commands_for_simulation(&board, &[id], None).unwrap();
        assert!(commands.is_empty());
    }
}
