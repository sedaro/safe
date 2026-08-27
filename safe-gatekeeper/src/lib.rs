use std::process::Stdio;

use anyhow::Context;
use safe::telemetry_frame::TelemetryFrame;
use safe_sim::{SedaroSimulator, SimulationResult};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub mod gatekeeper_types;

use crate::gatekeeper_types::{
    CheckAggregation, ComparisonOp, FieldCheck, GatekeeperConfig, GatekeeperInput,
    GatekeeperOutput, SimulationInputRequest, SimulationInputResponse,
};

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
    fn compare(&self, observed: f64, threshold: f64) -> bool {
        match self {
            ComparisonOp::Lt => observed < threshold,
            ComparisonOp::Lte => observed <= threshold,
            ComparisonOp::Gt => observed > threshold,
            ComparisonOp::Gte => observed >= threshold,
            ComparisonOp::Eq => (observed - threshold).abs() <= 1e-9,
            ComparisonOp::Ne => (observed - threshold).abs() > 1e-9,
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

    /// Runs the configured mission adapter once to materialize the EDS epoch
    /// and patches for the latest telemetry and requested command batch.
    async fn build_simulation_input(
        &self,
        request: &SimulationInputRequest,
    ) -> anyhow::Result<SimulationInputResponse> {
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

        let output = child
            .wait_with_output()
            .await
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
        request: &SimulationInputRequest,
    ) -> anyhow::Result<SimulationResult> {
        if self.config.sim_duration_days <= 0.0 {
            anyhow::bail!(
                "gatekeeper sim_duration_days must be > 0, got {}",
                self.config.sim_duration_days
            );
        }

        let input = self.build_simulation_input(request).await?;
        if !input.start_time_mjd.is_finite() {
            anyhow::bail!("simulation input adapter returned a non-finite start_time_mjd");
        }

        let simulator = self
            .simulator
            .clone()
            .at_epoch(input.start_time_mjd)
            .patch_multi(input.patches);

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

    fn evaluate(&self, result: &SimulationResult) -> anyhow::Result<String> {
        if !result.success {
            anyhow::bail!(
                "Simulation failed (code={:?}): {}",
                result.exit_code,
                result.stderr
            );
        }

        if self.config.field_checks.is_empty() {
            return Ok(format!(
                "Simulation OK (code={:?}); no field checks configured",
                result.exit_code
            ));
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
            if !check.op.compare(observed, check.threshold) {
                anyhow::bail!(
                    "Constraint violated: {}:{} {:?} must be {} {} (observed={:.6})",
                    check.target_file,
                    check.field,
                    check.aggregation,
                    check.op.as_str(),
                    check.threshold,
                    observed
                );
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

        Ok(format!(
            "Simulation OK (code={:?}); checks passed [{}]",
            result.exit_code,
            passed_checks.join("; ")
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

                    let request = SimulationInputRequest {
                        telemetry,
                        board,
                        candidate_command_ids: candidate_command_ids.clone(),
                        config: self.config.input_adapter_config.clone(),
                    };
                    let out = match self.run_simulation(&request).await {
                        Ok(result) => match self.evaluate(&result) {
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
            board: BoardState::default(),
            candidate_command_ids: Vec::new(),
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
}
