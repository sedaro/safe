use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use nalgebra::{Quaternion, UnitQuaternion, Vector3};
use safe::protocol::{Command, TimedCommand};
use safe::telemetry_frame::TelemetryFrame;
use safe::utils::{SECONDS_PER_DAY, gps_to_utc_mjd};
use safe_sim::{EdsFrame, EdsPatch, SimulationResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command as ProcessCommand;

use crate::planning::threat_is_within_range;
use crate::types::{
    CoorbitalEvasionMode, EdsPointingSchedule, GeometrySample, ModeScheduleEntry, PointingTarget,
    QuaternionScheduleEntry, ScheduledPointing, ThreatGeometry, ValidationReport,
};

const POINTING_MODE_SCHEDULE_TYPE: &str = "[(f64, str)]";
const POINTING_QUATERNION_SCHEDULE_TYPE: &str = "[(f64, (f64, f64, f64, f64))]";

fn telemetry_vec3(value: &Value) -> anyhow::Result<[f64; 3]> {
    let values = value
        .as_array()
        .filter(|values| values.len() == 3)
        .context("must be a three-element numeric array")?;
    let values = values
        .iter()
        .map(|value| {
            value
                .as_f64()
                .filter(|value| value.is_finite())
                .context("contains a non-finite value")
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok([values[0], values[1], values[2]])
}

/// Wire-compatible with the gatekeeper input adapter contract without making
/// this autonomy mode depend on an adapter implementation.
#[derive(Debug, Clone, Serialize)]
struct SimulationInputRequest {
    telemetry: TelemetryFrame,
    commands: Vec<TimedCommand>,
    config: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SimulationInput {
    pub(crate) start_time_mjd: f64,
    pub(crate) patches: Vec<EdsPatch>,
}

impl CoorbitalEvasionMode {
    fn add_command_to_schedule(
        &self,
        schedule: &mut EdsPointingSchedule,
        command: &Command,
        time_mjd: f64,
    ) {
        match command {
            Command::PointNadir => schedule
                .mode_schedule
                .push((time_mjd, self.config.nadir_mode_id.clone())),
            Command::PointSunYaw => schedule
                .mode_schedule
                .push((time_mjd, self.config.sun_yaw_mode_id.clone())),
            Command::PointQuaternion { x, y, z, w } => schedule
                .quaternion_schedule
                .push((time_mjd, (*x, *y, *z, *w))),
            _ => {}
        }
    }

    fn canonicalize_schedule(schedule: &mut EdsPointingSchedule) {
        schedule
            .mode_schedule
            .sort_by(|left, right| left.0.partial_cmp(&right.0).unwrap());
        schedule
            .quaternion_schedule
            .sort_by(|left, right| left.0.partial_cmp(&right.0).unwrap());
        schedule.mode_schedule.dedup_by(|left, right| {
            if left.0 == right.0 {
                *left = right.clone();
                true
            } else {
                false
            }
        });
        schedule.quaternion_schedule.dedup_by(|left, right| {
            if left.0 == right.0 {
                *left = right.clone();
                true
            } else {
                false
            }
        });
    }

    fn serialize_mode_schedule(schedule: &[ModeScheduleEntry]) -> String {
        let entries = schedule
            .iter()
            .map(|(time_mjd, mode_id)| {
                format!(
                    "({time_mjd:.15}, {})",
                    serde_json::to_string(mode_id).expect("serializing a string cannot fail")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("[{entries}]")
    }

    fn serialize_quaternion_schedule(schedule: &[QuaternionScheduleEntry]) -> String {
        let entries = schedule
            .iter()
            .map(|(time_mjd, (x, y, z, w))| {
                format!("({time_mjd:.15}, ({x:.15}, {y:.15}, {z:.15}, {w:.15}))")
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("[{entries}]")
    }

    pub(crate) fn accepted_pointing_schedule(
        &self,
        sim_start_mjd: f64,
        sim_end_mjd: f64,
    ) -> EdsPointingSchedule {
        let mut before_start: Option<(f64, Command)> = None;
        let mut schedule = EdsPointingSchedule::default();

        for command_id in &self.latest_board_snapshot.source_of_truth {
            let Some((_from, timed_command, _ts_mono)) =
                self.latest_board_snapshot.proposals.get(command_id)
            else {
                continue;
            };
            let (command, time_mjd) = match timed_command {
                TimedCommand::Now(command) => (command, sim_start_mjd),
                TimedCommand::Scheduled { cmd, gps_time } => {
                    let Some(time_mjd) = gps_to_utc_mjd(*gps_time) else {
                        continue;
                    };
                    (cmd, time_mjd)
                }
                TimedCommand::NOOP => continue,
            };
            if time_mjd > sim_end_mjd {
                continue;
            }
            if !matches!(
                command,
                Command::PointNadir | Command::PointSunYaw | Command::PointQuaternion { .. }
            ) {
                continue;
            }
            if time_mjd < sim_start_mjd {
                if before_start
                    .as_ref()
                    .is_none_or(|(previous_time, _)| time_mjd >= *previous_time)
                {
                    before_start = Some((time_mjd, command.clone()));
                }
            } else {
                self.add_command_to_schedule(&mut schedule, command, time_mjd);
            }
        }
        if let Some((_original_time, command)) = before_start {
            self.add_command_to_schedule(&mut schedule, &command, sim_start_mjd);
        }
        let has_active_mode = schedule
            .mode_schedule
            .iter()
            .any(|(time_mjd, _)| *time_mjd <= sim_start_mjd);
        let has_active_quaternion = schedule
            .quaternion_schedule
            .iter()
            .any(|(time_mjd, _)| *time_mjd <= sim_start_mjd);
        if !has_active_mode && !has_active_quaternion {
            schedule
                .mode_schedule
                .push((sim_start_mjd, self.config.nadir_mode_id.clone()));
        }
        Self::canonicalize_schedule(&mut schedule);
        schedule
    }

    pub(crate) fn selected_pointing_schedule(
        &self,
        accepted: &EdsPointingSchedule,
        earliest_command_mjd: f64,
        selected: &[ScheduledPointing],
    ) -> EdsPointingSchedule {
        let mut schedule = EdsPointingSchedule {
            mode_schedule: accepted
                .mode_schedule
                .iter()
                .filter(|(time_mjd, _)| *time_mjd < earliest_command_mjd)
                .cloned()
                .collect(),
            quaternion_schedule: accepted
                .quaternion_schedule
                .iter()
                .filter(|(time_mjd, _)| *time_mjd < earliest_command_mjd)
                .cloned()
                .collect(),
        };
        for command in selected {
            match &command.target {
                PointingTarget::Nadir => schedule
                    .mode_schedule
                    .push((command.time_mjd, self.config.nadir_mode_id.clone())),
                PointingTarget::Quaternion(quaternion) => {
                    let q = quaternion.quaternion();
                    schedule
                        .quaternion_schedule
                        .push((command.time_mjd, (q.i, q.j, q.k, q.w)));
                }
            }
        }
        Self::canonicalize_schedule(&mut schedule);
        schedule
    }

    fn mode_patches(&self, telemetry: &TelemetryFrame) -> anyhow::Result<Vec<EdsPatch>> {
        let mut patches = vec![
            EdsPatch::new(
                &self.config.agent_id,
                "gnc",
                "root!.gnc_time_step_limits",
                "(f64, f64)",
                &format!(
                    "({:.6}, {:.6})",
                    self.config.gnc_time_step_limits.0, self.config.gnc_time_step_limits.1
                ),
            ),
            EdsPatch::new(
                &self.config.agent_id,
                "cdh",
                "root!.cdh_time_step_limits",
                "(f64, f64)",
                &format!(
                    "({:.6}, {:.6})",
                    self.config.cdh_time_step_limits.0, self.config.cdh_time_step_limits.1
                ),
            ),
            EdsPatch::new(
                &self.config.agent_id,
                "power",
                "root!.power_time_step_limits",
                "(f64, f64)",
                &format!(
                    "({:.6}, {:.6})",
                    self.config.power_time_step_limits.0, self.config.power_time_step_limits.1
                ),
            ),
        ];

        let augmented = telemetry
            .payload
            .get("augmented")
            .and_then(Value::as_object)
            .context("telemetry is missing object payload.augmented")?;
        let ground = augmented
            .get("ground_threat_locations")
            .and_then(Value::as_object);
        let space = augmented
            .get("space_threat_epoch_states")
            .and_then(Value::as_object);

        for id in &self.config.threat_ids {
            match (
                ground.and_then(|locations| locations.get(id)),
                space.and_then(|states| states.get(id)),
            ) {
                (Some(_), Some(_)) => anyhow::bail!(
                    "telemetry provides both ground and space state for threat '{id}'"
                ),
                (Some(location), None) => {
                    let [latitude_deg, longitude_deg, altitude_km] = telemetry_vec3(location)
                        .with_context(|| format!("ground threat '{id}'"))?;
                    if !(-90.0..=90.0).contains(&latitude_deg)
                        || !(-180.0..=180.0).contains(&longitude_deg)
                    {
                        anyhow::bail!("ground threat '{id}' has an invalid latitude or longitude");
                    }
                    patches.push(EdsPatch::new(
                        &self.config.agent_id,
                        "gnc",
                        &format!("{id}.latitude_deg"),
                        "deg",
                        &format!("{latitude_deg:.15}"),
                    ));
                    patches.push(EdsPatch::new(
                        &self.config.agent_id,
                        "gnc",
                        &format!("{id}.longitude_deg"),
                        "deg",
                        &format!("{longitude_deg:.15}"),
                    ));
                    patches.push(EdsPatch::new(
                        &self.config.agent_id,
                        "gnc",
                        &format!("{id}.altitude_km"),
                        "f64",
                        &format!("{altitude_km:.15}"),
                    ));
                }
                (None, Some(state)) => {
                    let state = state
                        .as_array()
                        .filter(|state| state.len() == 3)
                        .with_context(|| {
                            format!("space threat '{id}' must be [epoch_mjd, position, velocity]")
                        })?;
                    let epoch_mjd = state[0]
                        .as_f64()
                        .filter(|value| value.is_finite())
                        .with_context(|| format!("space threat '{id}' has an invalid epoch"))?;
                    let position = telemetry_vec3(&state[1])
                        .with_context(|| format!("space threat '{id}' position"))?;
                    let velocity = telemetry_vec3(&state[2])
                        .with_context(|| format!("space threat '{id}' velocity"))?;
                    patches.push(EdsPatch::new(
                        &self.config.agent_id,
                        "gnc",
                        &format!("{id}.epoch_mjd"),
                        "f64",
                        &epoch_mjd.to_string(),
                    ));
                    patches.push(EdsPatch::new(
                        &self.config.agent_id,
                        "gnc",
                        &format!("{id}.epoch_position"),
                        "eci",
                        &format!("[{}, {}, {}]", position[0], position[1], position[2]),
                    ));
                    patches.push(EdsPatch::new(
                        &self.config.agent_id,
                        "gnc",
                        &format!("{id}.epoch_velocity"),
                        "#[f64; 3]",
                        &format!("[{}, {}, {}]", velocity[0], velocity[1], velocity[2]),
                    ));
                }
                (None, None) => {
                    anyhow::bail!("telemetry is missing state for configured threat '{id}'")
                }
            }
        }
        Ok(patches)
    }

    pub(crate) async fn simulation_input(
        &self,
        telemetry: &TelemetryFrame,
    ) -> anyhow::Result<SimulationInput> {
        let (executable, args) = self
            .config
            .input_adapter_command
            .split_first()
            .context("input_adapter_command is empty")?;
        let mut child = ProcessCommand::new(executable)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to start simulation input adapter '{executable}'"))?;
        let request = SimulationInputRequest {
            telemetry: telemetry.clone(),
            commands: Vec::new(),
            config: self.config.input_adapter_config.clone(),
        };
        let mut stdin = child.stdin.take().context("input adapter has no stdin")?;
        stdin.write_all(&serde_json::to_vec(&request)?).await?;
        stdin.write_all(b"\n").await?;
        drop(stdin);
        let output = tokio::time::timeout(
            Duration::from_secs(self.config.input_adapter_timeout_secs),
            child.wait_with_output(),
        )
        .await
        .with_context(|| {
            format!(
                "simulation input adapter timed out after {} seconds",
                self.config.input_adapter_timeout_secs
            )
        })??;
        if !output.status.success() {
            bail!(
                "simulation input adapter failed (code={:?}): {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let input: SimulationInput = serde_json::from_slice(&output.stdout).with_context(|| {
            format!(
                "simulation input adapter returned invalid JSON: {}",
                String::from_utf8_lossy(&output.stdout).trim()
            )
        })?;
        if !input.start_time_mjd.is_finite() {
            bail!("simulation input adapter returned a non-finite start_time_mjd");
        }
        Ok(input)
    }

    fn frames_for_result<'a>(
        &self,
        result: &'a SimulationResult,
    ) -> anyhow::Result<&'a [EdsFrame]> {
        if !result.success {
            anyhow::bail!(
                "simulation failed (code={:?}): {}",
                result.exit_code,
                result.stderr
            );
        }
        let target_file = format!(
            "{}.{}.jsonl",
            self.config.agent_id, self.config.result_engine
        );
        result
            .frames_by_file
            .get(&target_file)
            .map(Vec::as_slice)
            .or_else(|| {
                result
                    .frames_by_file
                    .iter()
                    .find(|(name, _)| name.ends_with(&target_file))
                    .map(|(_, frames)| frames.as_slice())
            })
            .with_context(|| format!("missing expected simulation target file '{target_file}'"))
    }

    fn field_vec(frame: &EdsFrame, field: &str, length: usize) -> anyhow::Result<Vec<f64>> {
        let datum = frame
            .get_by_field(field)
            .map_err(|error| anyhow::anyhow!("missing EDS result field '{field}': {error}"))?;
        let sequence = datum.data.as_seq().map_err(|error| {
            anyhow::anyhow!("EDS result field '{field}' is not a sequence: {error}")
        })?;
        if sequence.len() != length {
            anyhow::bail!(
                "EDS result field '{field}' has {} values; expected {length}",
                sequence.len()
            );
        }
        sequence
            .iter()
            .map(|value| {
                value.as_f64().map_err(|error| {
                    anyhow::anyhow!("EDS result field '{field}' contains a non-f64 value: {error}")
                })
            })
            .collect()
    }

    fn field_vec3(frame: &EdsFrame, field: &str) -> anyhow::Result<Vector3<f64>> {
        let values = Self::field_vec(frame, field, 3)?;
        Ok(Vector3::new(values[0], values[1], values[2]))
    }

    fn field_quaternion(frame: &EdsFrame, field: &str) -> anyhow::Result<UnitQuaternion<f64>> {
        let values = Self::field_vec(frame, field, 4)?;
        Ok(UnitQuaternion::new_normalize(Quaternion::new(
            values[3], values[0], values[1], values[2],
        )))
    }

    fn field_bool(frame: &EdsFrame, field: &str) -> anyhow::Result<bool> {
        frame
            .get_by_field(field)
            .map_err(|error| anyhow::anyhow!("missing EDS result field '{field}': {error}"))?
            .data
            .as_bool()
            .map_err(|error| anyhow::anyhow!("EDS result field '{field}' is not boolean: {error}"))
    }

    pub(crate) fn decode_geometry(
        &self,
        result: &SimulationResult,
    ) -> anyhow::Result<Vec<GeometrySample>> {
        self.frames_for_result(result)?
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                let time_mjd = frame
                    .get_by_field(&self.config.time_field)
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "missing EDS result field '{}' in sample {index}: {error}",
                            self.config.time_field,
                        )
                    })?
                    .data
                    .as_f64()
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "EDS result field '{}' is not f64 in sample {index}: {error}",
                            self.config.time_field
                        )
                    })?;
                let threats = self
                    .config
                    .threat_ids
                    .iter()
                    .map(|id| {
                        Ok(ThreatGeometry {
                            relative_position_eci: Self::field_vec3(
                                frame,
                                &format!("{id}.{}", self.config.relative_position_field),
                            )?,
                            line_of_sight: Self::field_bool(
                                frame,
                                &format!("{id}.{}", self.config.line_of_sight_field),
                            )?,
                            in_field_of_view: Self::field_bool(
                                frame,
                                &format!("{id}.{}", self.config.in_field_of_view_field),
                            )?,
                        })
                    })
                    .collect::<anyhow::Result<Vec<_>>>()
                    .with_context(|| {
                        format!("failed to decode threat geometry in sample {index}")
                    })?;
                Ok(GeometrySample {
                    time_mjd,
                    position_eci: Self::field_vec3(frame, &self.config.position_field)?,
                    velocity_eci: Self::field_vec3(frame, &self.config.velocity_field)?,
                    attitude_body_to_eci: Self::field_quaternion(
                        frame,
                        &self.config.attitude_field,
                    )?,
                    boresight_eci: Self::field_vec3(
                        frame,
                        &format!(
                            "{}.{}",
                            self.config.field_of_view_id, self.config.boresight_field
                        ),
                    )?,
                    threats,
                })
            })
            .collect()
    }

    pub(crate) async fn run_geometry_simulation(
        &self,
        input: &SimulationInput,
        schedule: &EdsPointingSchedule,
        telemetry: &TelemetryFrame,
    ) -> anyhow::Result<Vec<GeometrySample>> {
        let sim_start_mjd = input.start_time_mjd;
        let mut patches = input.patches.clone();
        patches.extend(self.mode_patches(telemetry)?);
        patches.push(EdsPatch::new(
            &self.config.agent_id,
            &self.config.schedule_patch_engine,
            &self.config.pointing_mode_schedule_field,
            POINTING_MODE_SCHEDULE_TYPE,
            &Self::serialize_mode_schedule(&schedule.mode_schedule),
        ));
        patches.push(EdsPatch::new(
            &self.config.agent_id,
            &self.config.schedule_patch_engine,
            &self.config.pointing_quaternion_schedule_field,
            POINTING_QUATERNION_SCHEDULE_TYPE,
            &Self::serialize_quaternion_schedule(&schedule.quaternion_schedule),
        ));
        let simulator = safe_sim::SedaroSimulator::new(&self.config.eds_path)
            .at_epoch(sim_start_mjd)
            .timeout(Duration::from_secs(self.config.simulation_timeout_secs))
            .patch_multi(patches);
        let workspace = simulator.workspace_dir().with_context(|| {
            format!(
                "failed to resolve coorbital-evasion EDS workspace from '{}'",
                self.config.eds_path.display()
            )
        })?;
        let result = simulator
            .run_collect(self.config.sim_duration_days)
            .await
            .with_context(|| {
                format!(
                    "coorbital-evasion simulation failed (workspace='{}')",
                    workspace.display()
                )
            })?;
        self.decode_geometry(&result)
    }

    pub(crate) fn validation_report(
        &self,
        samples: &[GeometrySample],
        earliest_command_mjd: f64,
        _fov_half_angle_rad: f64,
    ) -> ValidationReport {
        let mut report = ValidationReport::default();
        let earliest = samples
            .iter()
            .position(|sample| sample.time_mjd >= earliest_command_mjd)
            .unwrap_or(samples.len());
        for index in earliest..samples.len() {
            let sample = &samples[index];
            let mut exposed = false;
            for (threat_index, threat) in sample.threats.iter().enumerate() {
                if threat_is_within_range(threat, self.config.threat_max_range_km)
                    && threat.line_of_sight
                    && threat.in_field_of_view
                {
                    exposed = true;
                    let id = &self.config.threat_ids[threat_index];
                    if !report.exposed_threat_ids.contains(id) {
                        report.exposed_threat_ids.push(id.clone());
                    }
                }
            }
            if exposed {
                report.first_exposed_mjd.get_or_insert(sample.time_mjd);
                report.last_exposed_mjd = Some(sample.time_mjd);
                if let Some(next) = samples.get(index + 1) {
                    report.score.exposure_secs +=
                        (next.time_mjd - sample.time_mjd) * SECONDS_PER_DAY;
                }
            }
            if let Some(next) = samples.get(index + 1) {
                let nadir = -sample.position_eci.normalize();
                report.score.nadir_cost += (next.time_mjd - sample.time_mjd)
                    * SECONDS_PER_DAY
                    * (1.0 - sample.boresight_eci.normalize().dot(&nadir));
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use safe::protocol::{AutonomyModeBoardState, AutonomyModeId, BoardCmdId};
    use safe::telemetry_frame::TelemetryFrame;
    use safe::utils::utc_mjd_to_gps;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn mode_patches_use_ground_threat_state_from_telemetry() {
        let mut mode = CoorbitalEvasionMode::new();
        mode.config.threat_ids = vec!["ground-threat".to_string()];
        let telemetry = TelemetryFrame::new(serde_json::json!({
            "augmented": {
                "ground_threat_locations": {
                    "ground-threat": [-23.0, -67.0, 0.5]
                }
            }
        }));

        let patches = mode.mode_patches(&telemetry).unwrap();

        assert!(
            patches.iter().any(|patch| {
                patch.field == "ground-threat.latitude_deg" && patch.value == "-23.000000000000000"
            })
        );
        assert!(
            patches.iter().any(|patch| {
                patch.field == "ground-threat.longitude_deg" && patch.value == "-67.000000000000000"
            })
        );
        assert!(
            patches.iter().any(|patch| {
                patch.field == "ground-threat.altitude_km" && patch.value == "0.500000000000000"
            })
        );
    }

    #[test]
    fn mode_patches_use_space_threat_state_from_telemetry() {
        let mut mode = CoorbitalEvasionMode::new();
        mode.config.threat_ids = vec!["space-threat".to_string()];
        let telemetry = TelemetryFrame::new(serde_json::json!({
            "augmented": {
                "space_threat_epoch_states": {
                    "space-threat": [60000.0, [1.0, 2.0, 3.0], [4.0, 5.0, 6.0]]
                }
            }
        }));

        let patches = mode.mode_patches(&telemetry).unwrap();

        assert!(
            patches
                .iter()
                .any(|patch| { patch.field == "space-threat.epoch_mjd" && patch.value == "60000" })
        );
        assert!(patches.iter().any(|patch| {
            patch.field == "space-threat.epoch_position" && patch.value == "[1, 2, 3]"
        }));
        assert!(patches.iter().any(|patch| {
            patch.field == "space-threat.epoch_velocity" && patch.value == "[4, 5, 6]"
        }));
    }

    #[test]
    fn mode_patches_reject_missing_configured_threat_state() {
        let mut mode = CoorbitalEvasionMode::new();
        mode.config.threat_ids = vec!["missing-threat".to_string()];
        let telemetry = TelemetryFrame::new(serde_json::json!({"augmented": {}}));

        assert!(
            mode.mode_patches(&telemetry)
                .unwrap_err()
                .to_string()
                .contains("missing state for configured threat 'missing-threat'")
        );
    }

    #[test]
    fn accepted_mixed_schedule_preserves_latest_pre_epoch_target() {
        let start = 60_000.0;
        let mut mode = CoorbitalEvasionMode::new();
        let mut board = AutonomyModeBoardState::default();
        let from = AutonomyModeId(Uuid::nil());
        let commands = [
            (start - 0.02, Command::PointNadir),
            (start - 0.01, Command::PointSunYaw),
            (start + 0.01, Command::PointNadir),
            (
                start + 0.02,
                Command::PointQuaternion {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                    w: 1.0,
                },
            ),
        ];
        for (index, (time_mjd, command)) in commands.into_iter().enumerate() {
            let id = BoardCmdId(format!("{index}"));
            board.proposals.insert(
                id.clone(),
                (
                    from,
                    TimedCommand::Scheduled {
                        cmd: command,
                        gps_time: utc_mjd_to_gps(time_mjd).unwrap(),
                    },
                    index as u64,
                ),
            );
            board.source_of_truth.push(id);
        }
        mode.latest_board_snapshot = board;

        let schedule = mode.accepted_pointing_schedule(start, start + 0.03);
        let future_mode_time = schedule.mode_schedule[1].0;
        assert_eq!(
            schedule.mode_schedule,
            vec![
                (start, mode.config.sun_yaw_mode_id.clone()),
                (future_mode_time, mode.config.nadir_mode_id.clone()),
            ]
        );
        assert_eq!(schedule.quaternion_schedule.len(), 1);
        assert!((schedule.quaternion_schedule[0].0 - (start + 0.02)).abs() < 1.0e-9);
    }

    #[test]
    fn empty_accepted_schedule_starts_nadir() {
        let mode = CoorbitalEvasionMode::new();
        let schedule = mode.accepted_pointing_schedule(60_000.0, 60_001.0);
        assert_eq!(
            schedule.mode_schedule,
            vec![(60_000.0, mode.config.nadir_mode_id.clone())]
        );
        assert!(schedule.quaternion_schedule.is_empty());
    }

    #[test]
    fn future_only_accepted_schedule_starts_nadir() {
        let start = 60_000.0;
        let mut mode = CoorbitalEvasionMode::new();
        let mut board = AutonomyModeBoardState::default();
        let id = BoardCmdId("future".to_string());
        board.proposals.insert(
            id.clone(),
            (
                AutonomyModeId(Uuid::nil()),
                TimedCommand::Scheduled {
                    cmd: Command::PointSunYaw,
                    gps_time: utc_mjd_to_gps(start + 0.01).unwrap(),
                },
                0,
            ),
        );
        board.source_of_truth.push(id);
        mode.latest_board_snapshot = board;

        let schedule = mode.accepted_pointing_schedule(start, start + 0.02);

        let future_mode_time = schedule.mode_schedule[1].0;
        assert_eq!(
            schedule.mode_schedule,
            vec![
                (start, mode.config.nadir_mode_id.clone()),
                (future_mode_time, mode.config.sun_yaw_mode_id.clone()),
            ]
        );
        assert!(schedule.quaternion_schedule.is_empty());
    }

    #[test]
    fn selected_schedule_keeps_only_pre_command_accepted_entries() {
        let mode = CoorbitalEvasionMode::new();
        let accepted = EdsPointingSchedule {
            mode_schedule: vec![(1.0, "sun".to_string()), (2.0, "thruster".to_string())],
            quaternion_schedule: Vec::new(),
        };
        let selected = vec![ScheduledPointing {
            time_mjd: 1.5,
            target: PointingTarget::Nadir,
        }];
        let result = mode.selected_pointing_schedule(&accepted, 1.5, &selected);
        assert_eq!(
            result.mode_schedule,
            vec![(1.0, "sun".to_string()), (1.5, mode.config.nadir_mode_id)]
        );
        assert!(result.quaternion_schedule.is_empty());
    }

    #[test]
    fn validation_ignores_threats_beyond_max_range() {
        let mut mode = CoorbitalEvasionMode::new();
        mode.config.threat_ids = vec!["threat".to_string()];
        mode.config.threat_max_range_km = 9.0;
        let samples = (0..=1)
            .map(|index| GeometrySample {
                time_mjd: 60_000.0 + index as f64 / SECONDS_PER_DAY,
                position_eci: Vector3::z(),
                velocity_eci: Vector3::y(),
                attitude_body_to_eci: UnitQuaternion::identity(),
                boresight_eci: Vector3::z(),
                threats: vec![ThreatGeometry {
                    relative_position_eci: -10.0 * Vector3::z(),
                    line_of_sight: true,
                    in_field_of_view: true,
                }],
            })
            .collect::<Vec<_>>();

        let report = mode.validation_report(&samples, 60_000.0, 30_f64.to_radians());
        assert_eq!(report.score.exposure_secs, 0.0);
        assert!(report.exposed_threat_ids.is_empty());
    }
}
