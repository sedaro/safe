use std::collections::HashSet;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use safe::protocol::TimedCommand;
use safe::telemetry_frame::TelemetryFrame;
use safe_sim::{EdsFrame, EdsPatch, MonteCarloStudy, SedaroSimulator, SimulationResult};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::{CheckAggregation, ComparisonOp, FieldCheck, MissionPlanningConfig};
use crate::planning::PlanningSample;

/// Wire-compatible with the gatekeeper input adapter contract without making
/// this autonomy mode depend on gatekeeper implementation code.
#[derive(Debug, Clone, Serialize)]
struct SimulationInputRequest {
    telemetry: TelemetryFrame,
    commands: Vec<TimedCommand>,
    config: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
struct SimulationInputResponse {
    start_time_mjd: f64,
    patches: Vec<EdsPatch>,
}

pub(crate) async fn run_schedule(
    config: &MissionPlanningConfig,
    telemetry: &TelemetryFrame,
    commands: Vec<TimedCommand>,
) -> Result<(f64, SimulationResult)> {
    let input = invoke_adapter(config, telemetry, commands).await?;
    let result = run_patches(config, input.start_time_mjd, input.patches).await?;
    Ok((input.start_time_mjd, result))
}

/// Runs the nominal and configured uncertainty simulations for a proposed schedule.
pub(crate) async fn validate_candidate_schedule(
    config: &MissionPlanningConfig,
    telemetry: &TelemetryFrame,
    commands: Vec<TimedCommand>,
) -> Result<()> {
    let input = invoke_adapter(config, telemetry, commands).await?;
    if !input.start_time_mjd.is_finite() {
        bail!("simulation input adapter returned a non-finite start_time_mjd");
    }
    let nominal = run_patches(config, input.start_time_mjd, input.patches.clone())
        .await
        .context("candidate-command simulation")?;
    validate_result(config, &nominal)?;
    drop(nominal);
    let Some(monte_carlo) = &config.monte_carlo else {
        return Ok(());
    };
    reject_conflicting_monte_carlo_targets(&input.patches, monte_carlo)?;
    let simulator = SedaroSimulator::new(&config.eds_path)
        .at_epoch(input.start_time_mjd)
        .patch_multi(input.patches)
        .timeout(Duration::from_secs(config.simulation_timeout_secs));
    let mut study = MonteCarloStudy::new(
        simulator.clone(),
        config.planning_horizon_secs / safe::utils::SECONDS_PER_DAY,
    )
    .samples(monte_carlo.samples)
    .seed(monte_carlo.seed);
    for parameter in &monte_carlo.parameters {
        study = study.parameter(parameter.clone());
    }
    let mut passed = 0;
    let mut failures = Vec::new();
    // Run cases one at a time so their decoded EDS outputs are released before
    // the next sample. Retaining a complete study can exhaust a flight host.
    for case in study.generate_cases()? {
        let id = case.id;
        let result = simulator
            .clone()
            .patch_multi(case.patches)
            .run_collect(config.planning_horizon_secs / safe::utils::SECONDS_PER_DAY)
            .await;
        match result {
            Ok(result) if result.success => match validate_result(config, &result) {
                Ok(()) => passed += 1,
                Err(error) => failures.push(format!("{id}: {error}")),
            },
            Ok(_) => failures.push(format!("{id}: EDS simulation failed")),
            Err(error) => failures.push(format!("{id}: {error:#}")),
        }
    }
    let fraction = passed as f64 / monte_carlo.samples as f64;
    if fraction < monte_carlo.minimum_pass_fraction {
        bail!(
            "monte_carlo rejected candidate: {passed}/{} samples passed ({fraction:.3}, required {:.3}); failures [{}]",
            monte_carlo.samples,
            monte_carlo.minimum_pass_fraction,
            failures.into_iter().take(5).collect::<Vec<_>>().join("; ")
        );
    }
    Ok(())
}

fn reject_conflicting_monte_carlo_targets(
    baseline_patches: &[EdsPatch],
    monte_carlo: &crate::config::MonteCarloConfig,
) -> Result<()> {
    let baseline_targets = baseline_patches
        .iter()
        .map(|patch| (&patch.agent_id, &patch.engine, &patch.field))
        .collect::<HashSet<_>>();
    for parameter in &monte_carlo.parameters {
        let target = &parameter.target;
        if baseline_targets.contains(&(&target.agent_id, &target.engine, &target.field)) {
            bail!(
                "monte_carlo parameter '{}' targets an adapter patch; safe_sim::MonteCarloStudy cannot replace adapter-produced patches",
                parameter.name
            );
        }
    }
    Ok(())
}

async fn run_patches(
    config: &MissionPlanningConfig,
    start_time_mjd: f64,
    patches: Vec<EdsPatch>,
) -> Result<SimulationResult> {
    if !start_time_mjd.is_finite() {
        bail!("simulation input adapter returned a non-finite start_time_mjd");
    }
    let simulator = SedaroSimulator::new(&config.eds_path)
        .at_epoch(start_time_mjd)
        .patch_multi(patches)
        .timeout(Duration::from_secs(config.simulation_timeout_secs));
    let result = simulator
        .run_collect(config.planning_horizon_secs / safe::utils::SECONDS_PER_DAY)
        .await
        .context("EDS execution or output collection failed")?;
    if !result.success {
        bail!(
            "EDS simulation failed (code={:?}): {}",
            result.exit_code,
            result.stderr.trim()
        );
    }
    Ok(result)
}

async fn invoke_adapter(
    config: &MissionPlanningConfig,
    telemetry: &TelemetryFrame,
    commands: Vec<TimedCommand>,
) -> Result<SimulationInputResponse> {
    let (executable, args) = config
        .input_adapter_command
        .split_first()
        .context("input_adapter_command is empty")?;
    let mut child = Command::new(executable)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to start simulation input adapter '{executable}'"))?;

    let request = SimulationInputRequest {
        telemetry: telemetry.clone(),
        commands,
        config: config.input_adapter_config.clone(),
    };
    let encoded = serde_json::to_vec(&request)?;
    let mut stdin = child.stdin.take().context("input adapter has no stdin")?;
    stdin.write_all(&encoded).await?;
    stdin.write_all(b"\n").await?;
    drop(stdin);

    let output = tokio::time::timeout(
        Duration::from_secs(config.input_adapter_timeout_secs),
        child.wait_with_output(),
    )
    .await
    .with_context(|| {
        format!(
            "simulation input adapter timed out after {} seconds",
            config.input_adapter_timeout_secs
        )
    })??;
    if !output.status.success() {
        bail!(
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

pub(crate) fn extract_planning_samples(
    config: &MissionPlanningConfig,
    result: &SimulationResult,
) -> Result<Vec<PlanningSample>> {
    let frames = result_frames(result, &config.result_file)?;
    let state_of_charge_frames =
        frames_with_field(result, &config.time_field, &config.state_of_charge_field)?;
    let target_frames = config
        .targets
        .iter()
        .map(|target| {
            frames_with_field(result, &config.time_field, &target.in_fov_field)
                .with_context(|| format!("target '{}'", target.name))
        })
        .collect::<Result<Vec<_>>>()?;
    let station_frames = config
        .ground_stations
        .iter()
        .map(|station| {
            frames_with_field(result, &config.time_field, &station.elevation_field)
                .with_context(|| format!("ground station '{}'", station.name))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut samples = Vec::with_capacity(frames.len());
    for (index, frame) in frames.iter().enumerate() {
        let time_mjd =
            numeric(frame, &config.time_field).with_context(|| format!("sample {index} time"))?;
        let state_of_charge = numeric(
            frame_at_time(&state_of_charge_frames, time_mjd)
                .context("state of charge has no simulation frame")?,
            &config.state_of_charge_field,
        )
        .with_context(|| format!("sample {index} state of charge"))?;
        if !time_mjd.is_finite() || !state_of_charge.is_finite() {
            bail!("sample {index} contains non-finite time or state of charge");
        }

        let target_visible = config.targets.iter().zip(&target_frames).try_fold(
            false,
            |visible, (target, target_frames)| {
                boolean(
                    frame_at_time(target_frames, time_mjd).with_context(|| {
                        format!("target '{}' has no simulation frame", target.name)
                    })?,
                    &target.in_fov_field,
                )
                .with_context(|| format!("sample {index} target '{}'", target.name))
                .map(|value| visible || value)
            },
        )?;
        let station_elevations_deg = config
            .ground_stations
            .iter()
            .zip(&station_frames)
            .map(|(station, station_frames)| {
                numeric(
                    frame_at_time(station_frames, time_mjd).with_context(|| {
                        format!("ground station '{}' has no simulation frame", station.name)
                    })?,
                    &station.elevation_field,
                )
                .with_context(|| format!("sample {index} station '{}'", station.name))
            })
            .collect::<Result<Vec<_>>>()?;
        if station_elevations_deg
            .iter()
            .any(|value| !value.is_finite())
        {
            bail!("sample {index} contains a non-finite station elevation");
        }
        samples.push(PlanningSample {
            time_mjd,
            state_of_charge,
            target_visible,
            station_elevations_deg,
        });
    }
    samples.sort_by(|left, right| left.time_mjd.total_cmp(&right.time_mjd));
    Ok(samples)
}

pub(crate) fn validate_result(
    config: &MissionPlanningConfig,
    result: &SimulationResult,
) -> Result<()> {
    for check in &config.validation_checks {
        let values = result_frames(result, &check.target_file)?
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                numeric(frame, &check.field).with_context(|| {
                    format!(
                        "validation sample {index} is missing numeric field '{}:{}'",
                        check.target_file, check.field
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if values.is_empty() {
            bail!(
                "validation check found no numeric values for '{}:{}'",
                check.target_file,
                check.field
            );
        }
        let observed = aggregate(&values, check)?;
        if !compare(observed, check) {
            bail!(
                "validation check failed for '{}:{}' (observed {}, threshold {})",
                check.target_file,
                check.field,
                observed,
                check.threshold
            );
        }
    }
    Ok(())
}

fn result_frames<'a>(result: &'a SimulationResult, target_file: &str) -> Result<&'a [EdsFrame]> {
    result
        .frames_by_file
        .get(target_file)
        .or_else(|| {
            result
                .frames_by_file
                .iter()
                .find(|(name, _)| name.ends_with(target_file))
                .map(|(_, frames)| frames)
        })
        .map(Vec::as_slice)
        .with_context(|| format!("missing simulation result file '{target_file}'"))
}

fn frames_with_field<'a>(
    result: &'a SimulationResult,
    time_field: &str,
    field: &str,
) -> Result<Vec<(f64, &'a EdsFrame)>> {
    let mut frames = result
        .frames_by_file
        .values()
        .flat_map(|frames| frames.iter())
        .filter(|frame| frame.get_by_field(field).is_ok())
        .filter_map(|frame| {
            numeric(frame, time_field)
                .ok()
                .map(|frame_time| (frame_time, frame))
        })
        .collect::<Vec<_>>();
    frames.sort_by(|left, right| left.0.total_cmp(&right.0));
    if frames.is_empty() {
        bail!("missing simulation field '{field}'");
    }
    Ok(frames)
}

fn frame_at_time<'a>(frames: &[(f64, &'a EdsFrame)], time_mjd: f64) -> Option<&'a EdsFrame> {
    let index = frames.partition_point(|(frame_time, _)| *frame_time < time_mjd);
    match (frames.get(index.wrapping_sub(1)), frames.get(index)) {
        (Some(left), Some(right)) => {
            if (time_mjd - left.0).abs() <= (right.0 - time_mjd).abs() {
                Some(left.1)
            } else {
                Some(right.1)
            }
        }
        (Some(left), None) => Some(left.1),
        (None, Some(right)) => Some(right.1),
        (None, None) => None,
    }
}

fn numeric(frame: &EdsFrame, field: &str) -> Result<f64> {
    frame.get_by_field(field)?.data.as_f64()
}

fn boolean(frame: &EdsFrame, field: &str) -> Result<bool> {
    let value = &frame.get_by_field(field)?.data;
    match value {
        safe_sim::EdsValue::Bool(value) => Ok(*value),
        safe_sim::EdsValue::Sequence(values) => values
            .iter()
            .map(safe_sim::EdsValue::as_bool)
            .collect::<Result<Vec<_>>>()
            .map(|values| values.into_iter().any(|value| value)),
        _ => bail!("field '{field}' must be a boolean or an array of booleans"),
    }
}

fn aggregate(values: &[f64], check: &FieldCheck) -> Result<f64> {
    if values.iter().any(|value| !value.is_finite()) {
        bail!("validation field contains a non-finite value");
    }
    Ok(match check.aggregation {
        CheckAggregation::Last => *values.last().expect("values checked as non-empty"),
        CheckAggregation::Min => values.iter().copied().fold(f64::INFINITY, f64::min),
        CheckAggregation::Max => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        CheckAggregation::Mean => values.iter().sum::<f64>() / values.len() as f64,
    })
}

fn compare(observed: f64, check: &FieldCheck) -> bool {
    match check.op {
        ComparisonOp::Lt => observed < check.threshold,
        ComparisonOp::Lte => observed <= check.threshold,
        ComparisonOp::Gt => observed > check.threshold,
        ComparisonOp::Gte => observed >= check.threshold,
        ComparisonOp::Eq => (observed - check.threshold).abs() <= check.tolerance,
        ComparisonOp::Ne => (observed - check.threshold).abs() > check.tolerance,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use safe_sim::{EdsType, EdsValue};

    use super::*;
    use crate::config::{GroundStationConfig, TargetConfig};

    fn frame(time_mjd: f64, fields: &[(&str, EdsValue)]) -> EdsFrame {
        let signature = format!(
            "(time: f64, {})",
            fields
                .iter()
                .map(|(name, value)| match value {
                    EdsValue::F64(_) => format!("{name}: f64"),
                    EdsValue::Bool(_) => format!("{name}: bool"),
                    EdsValue::Sequence(_) => format!("{name}: [bool]"),
                    _ => panic!("unsupported test field value"),
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
        let values = std::iter::once(EdsValue::F64(time_mjd))
            .chain(fields.iter().map(|(_, value)| value.clone()))
            .collect();
        EdsFrame::new(
            EdsType::parse(&signature).unwrap(),
            EdsValue::Sequence(values),
        )
    }

    #[test]
    fn extracts_nearest_frames_from_interleaved_result_files() {
        let result = SimulationResult {
            success: true,
            frames_by_file: HashMap::from([
                (
                    "spacecraft.power.jsonl".to_string(),
                    vec![
                        frame(30.0, &[("soc", EdsValue::F64(0.8))]),
                        frame(10.0, &[("soc", EdsValue::F64(0.5))]),
                        frame(20.0, &[("soc", EdsValue::F64(0.7))]),
                        frame(0.0, &[("soc", EdsValue::F64(0.4))]),
                    ],
                ),
                (
                    "spacecraft.gnc.jsonl".to_string(),
                    vec![
                        frame(19.0, &[("target_visible", EdsValue::Bool(false))]),
                        frame(
                            9.0,
                            &[(
                                "target_visible",
                                EdsValue::Sequence(vec![
                                    EdsValue::Bool(false),
                                    EdsValue::Bool(true),
                                ]),
                            )],
                        ),
                    ],
                ),
                (
                    "spacecraft.cdh.jsonl".to_string(),
                    vec![
                        frame(22.0, &[("station_elevation", EdsValue::F64(15.0))]),
                        frame(8.0, &[("station_elevation", EdsValue::F64(5.0))]),
                    ],
                ),
            ]),
            ..SimulationResult::default()
        };
        let config = MissionPlanningConfig {
            result_file: "spacecraft.power.jsonl".to_string(),
            time_field: "time".to_string(),
            state_of_charge_field: "soc".to_string(),
            targets: vec![TargetConfig {
                name: "target".to_string(),
                in_fov_field: "target_visible".to_string(),
            }],
            ground_stations: vec![GroundStationConfig {
                name: "station".to_string(),
                latitude_deg: 0.0,
                longitude_deg: 0.0,
                altitude_m: 0.0,
                elevation_field: "station_elevation".to_string(),
            }],
            ..MissionPlanningConfig::default()
        };

        let samples = extract_planning_samples(&config, &result).unwrap();

        assert_eq!(samples.len(), 4);
        assert_eq!(
            samples
                .iter()
                .map(|sample| sample.time_mjd)
                .collect::<Vec<_>>(),
            vec![0.0, 10.0, 20.0, 30.0]
        );
        assert_eq!(
            samples
                .iter()
                .map(|sample| sample.state_of_charge)
                .collect::<Vec<_>>(),
            vec![0.4, 0.5, 0.7, 0.8]
        );
        assert_eq!(
            samples
                .iter()
                .map(|sample| sample.target_visible)
                .collect::<Vec<_>>(),
            vec![true, true, false, false]
        );
        assert_eq!(
            samples
                .iter()
                .map(|sample| sample.station_elevations_deg.clone())
                .collect::<Vec<_>>(),
            vec![vec![5.0], vec![5.0], vec![15.0], vec![15.0]]
        );
    }
}
