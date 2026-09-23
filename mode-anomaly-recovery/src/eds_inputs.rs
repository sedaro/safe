//! Trusted, deployment-configured EDS state, pointing and compute-power bindings.
//! IDs and frame conventions are supplied by configuration, never by the LLM.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow, bail, ensure};
use safe_sim::{EdsPatch, EdsType};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{AllowedAction, SimulationConfig, SimulationScenario, SimulationScenarioRole};
use crate::runtime::value_at_payload_path;
use crate::types::TelemetrySample;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SimulationInitialization {
    pub(crate) source: String,
    pub(crate) epoch_path: String,
    pub(crate) patches: Vec<StatePatch>,
    #[serde(default)]
    pub(crate) requirements: Vec<InputRequirement>,
    #[serde(default)]
    pub(crate) command_schedules: Vec<CommandSchedule>,
    #[serde(default)]
    pub(crate) compute_power_bindings: Vec<ComputePowerBinding>,
}

/// A constant-watt compute load for the simulation horizon. Deployment supplies
/// the EDS field and measured on/off draws; no spacecraft IDs live in host code.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ComputePowerBinding {
    pub(crate) id: String,
    pub(crate) agent_id: String,
    pub(crate) engine: String,
    pub(crate) field: String,
    pub(crate) operating_power_w: f64,
    pub(crate) shutdown_power_w: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InputRequirement {
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) expected: Option<Value>,
    #[serde(default)]
    pub(crate) min: Option<f64>,
    #[serde(default)]
    pub(crate) max: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StatePatch {
    pub(crate) agent_id: String,
    pub(crate) engine: String,
    pub(crate) field: String,
    #[serde(rename = "type")]
    pub(crate) type_: String,
    #[serde(default)]
    pub(crate) value: Option<Value>,
    #[serde(default)]
    pub(crate) telemetry_path: Option<String>,
    #[serde(default)]
    pub(crate) telemetry_paths: Vec<String>,
    #[serde(default = "unit_scale")]
    pub(crate) scale: f64,
}

fn unit_scale() -> f64 {
    1.0
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommandSchedule {
    pub(crate) id: String,
    pub(crate) agent_id: String,
    pub(crate) engines: Vec<String>,
    pub(crate) field: String,
    pub(crate) action_modes: HashMap<AllowedAction, String>,
}

fn valid_path(path: &str) -> bool {
    !path.is_empty() && path.split('.').all(|p| !p.is_empty() && p.trim() == p)
}

impl SimulationInitialization {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            !self.source.trim().is_empty(),
            "initialization source is required"
        );
        ensure!(
            valid_path(&self.epoch_path),
            "invalid initialization epoch_path"
        );
        ensure!(
            !self.patches.is_empty(),
            "initialization patches are required"
        );
        ensure!(
            !self.command_schedules.is_empty() || !self.compute_power_bindings.is_empty(),
            "executable command or compute-power bindings are required"
        );
        let mut targets = HashSet::new();
        for patch in &self.patches {
            ensure!(
                !patch.agent_id.trim().is_empty()
                    && !patch.engine.trim().is_empty()
                    && !patch.field.trim().is_empty(),
                "initialization patch target is required"
            );
            ensure!(
                targets.insert((&patch.agent_id, &patch.engine, &patch.field)),
                "duplicate initialization target"
            );
            let sources = usize::from(patch.value.is_some())
                + usize::from(patch.telemetry_path.is_some())
                + usize::from(!patch.telemetry_paths.is_empty());
            ensure!(
                sources == 1,
                "state patch requires exactly one value or telemetry source"
            );
            ensure!(
                patch.scale.is_finite() && patch.scale != 0.0,
                "invalid state patch scale"
            );
            for path in patch
                .telemetry_path
                .iter()
                .chain(patch.telemetry_paths.iter())
            {
                ensure!(valid_path(path), "invalid state patch telemetry path");
            }
            EdsType::parse(&patch.type_)?;
            let compact = patch.type_.replace(' ', "");
            ensure!(
                matches!(compact.as_str(), "f64" | "bool" | "#[f64;3]" | "#[f64;4]")
                    || patch
                        .value
                        .as_ref()
                        .is_some_and(|v| v.as_array().is_some_and(Vec::is_empty))
                        && compact.starts_with('['),
                "state patches support f64, bool, fixed 3/4-vectors, or constant empty lists"
            );
            if !patch.telemetry_paths.is_empty() {
                ensure!(
                    (compact == "#[f64;3]" && patch.telemetry_paths.len() == 3)
                        || (compact == "#[f64;4]" && patch.telemetry_paths.len() == 4),
                    "component paths must match the fixed vector length"
                );
            }
            if let Some(value) = &patch.value {
                serialize_state_value(&patch.type_, value, patch.scale)?;
            }
        }
        for requirement in &self.requirements {
            ensure!(
                valid_path(&requirement.path),
                "invalid input requirement path"
            );
            ensure!(
                requirement.expected.is_some()
                    != (requirement.min.is_some() || requirement.max.is_some()),
                "input requirement needs either expected or numeric bounds"
            );
            ensure!(
                requirement.expected.as_ref().is_none_or(|v| !v.is_null()),
                "null expectation is not usable input"
            );
            ensure!(
                requirement
                    .min
                    .into_iter()
                    .chain(requirement.max)
                    .all(f64::is_finite),
                "input bounds must be finite"
            );
            if let (Some(min), Some(max)) = (requirement.min, requirement.max) {
                ensure!(min <= max, "input minimum exceeds maximum");
            }
        }
        let mut ids = HashSet::new();
        for binding in &self.compute_power_bindings {
            ensure!(
                !binding.id.trim().is_empty()
                    && ids.insert(&binding.id)
                    && !binding.agent_id.trim().is_empty()
                    && !binding.engine.trim().is_empty()
                    && !binding.field.trim().is_empty(),
                "invalid compute-power binding"
            );
            ensure!(
                binding.operating_power_w.is_finite()
                    && binding.shutdown_power_w.is_finite()
                    && binding.shutdown_power_w >= 0.0
                    && binding.operating_power_w > binding.shutdown_power_w,
                "compute power must be finite, nonnegative and lower after shutdown"
            );
            ensure!(
                targets.insert((&binding.agent_id, &binding.engine, &binding.field)),
                "duplicate compute/state/schedule target"
            );
        }
        for schedule in &self.command_schedules {
            ensure!(
                !schedule.id.trim().is_empty()
                    && ids.insert(&schedule.id)
                    && !schedule.agent_id.trim().is_empty()
                    && !schedule.field.trim().is_empty()
                    && !schedule.engines.is_empty()
                    && !schedule.action_modes.is_empty(),
                "invalid command schedule binding"
            );
            for engine in &schedule.engines {
                ensure!(
                    !engine.trim().is_empty(),
                    "command schedule engine is required"
                );
                ensure!(
                    targets.insert((&schedule.agent_id, engine, &schedule.field)),
                    "duplicate state/schedule target"
                );
            }
            let mut modes = HashSet::new();
            for (action, mode) in &schedule.action_modes {
                ensure!(
                    matches!(
                        action,
                        AllowedAction::PointNadir | AllowedAction::PointSunYaw
                    ) && !mode.trim().is_empty()
                        && modes.insert(mode),
                    "pointing schedules require distinct non-empty mode IDs for supported actions"
                );
            }
        }
        Ok(())
    }
}

fn number(value: &Value) -> Result<f64> {
    value
        .as_f64()
        .filter(|n| n.is_finite())
        .ok_or_else(|| anyhow!("expected a finite number"))
}

fn serialize_state_value(type_: &str, value: &Value, scale: f64) -> Result<String> {
    let compact = type_.replace(' ', "");
    if compact == "bool" {
        ensure!(scale == 1.0, "boolean patches cannot be scaled");
        return value
            .as_bool()
            .map(|v| v.to_string())
            .ok_or_else(|| anyhow!("expected a boolean"));
    }
    if compact == "f64" {
        let scaled = number(value)? * scale;
        ensure!(scaled.is_finite(), "scaled state value is non-finite");
        return Ok(format!("{scaled:?}"));
    }
    let values = value
        .as_array()
        .ok_or_else(|| anyhow!("expected an array"))?;
    if matches!(compact.as_str(), "#[f64;3]" | "#[f64;4]") {
        let length = if compact == "#[f64;3]" { 3 } else { 4 };
        ensure!(values.len() == length, "state vector length mismatch");
        let scaled = values
            .iter()
            .map(|v| {
                let n = number(v)? * scale;
                ensure!(n.is_finite(), "scaled state vector is non-finite");
                Ok(n)
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok(serde_json::to_string(&scaled)?);
    }
    ensure!(
        compact.starts_with('[') && values.is_empty() && scale == 1.0,
        "only empty constant schedule lists are supported here"
    );
    Ok("[]".into())
}

pub(crate) struct PreparedInputs {
    pub(crate) epoch_mjd: Option<f64>,
    pub(crate) patches: Vec<EdsPatch>,
}

pub(crate) fn prepare_inputs(
    simulation: &SimulationConfig,
    scenario: &SimulationScenario,
    telemetry: &TelemetrySample,
    scenario_patches: Vec<EdsPatch>,
) -> Result<PreparedInputs> {
    let Some(initialization) = &simulation.initialization else {
        ensure!(
            scenario.role != Some(SimulationScenarioRole::Recovery),
            "recovery requires executable initialization and command schedule bindings"
        );
        return Ok(PreparedInputs {
            epoch_mjd: None,
            patches: scenario_patches,
        });
    };
    ensure!(
        telemetry.source.as_deref() == Some(initialization.source.as_str()),
        "simulation telemetry source mismatch"
    );
    let lookup = |path: &str| -> Result<Value> {
        value_at_payload_path(&telemetry.payload, path)
            .filter(|v| !v.is_null())
            .cloned()
            .ok_or_else(|| anyhow!("required simulation input '{path}' is missing or null"))
    };
    for binding in &scenario.state_bindings {
        ensure!(
            binding.source == initialization.source,
            "scenario state source mismatch"
        );
        lookup(&binding.path)?;
    }
    for requirement in &initialization.requirements {
        let value = lookup(&requirement.path)?;
        let satisfied = if let Some(expected) = &requirement.expected {
            value == *expected
        } else {
            let n = number(&value)?;
            requirement.min.is_none_or(|min| n >= min) && requirement.max.is_none_or(|max| n <= max)
        };
        ensure!(
            satisfied,
            "simulation input requirement failed: {}",
            requirement.path
        );
    }
    let epoch = number(&lookup(&initialization.epoch_path)?)?;
    let mut patches = Vec::new();
    for patch in &initialization.patches {
        let value = if let Some(value) = &patch.value {
            value.clone()
        } else if let Some(path) = &patch.telemetry_path {
            lookup(path)?
        } else {
            Value::Array(
                patch
                    .telemetry_paths
                    .iter()
                    .map(|p| lookup(p))
                    .collect::<Result<_>>()?,
            )
        };
        patches.push(EdsPatch::new(
            &patch.agent_id,
            &patch.engine,
            &patch.field,
            &patch.type_,
            &serialize_state_value(&patch.type_, &value, patch.scale)?,
        ));
    }
    let selected = match scenario.role {
        Some(SimulationScenarioRole::Baseline) => {
            ensure!(
                scenario.modeled_action.is_none()
                    && scenario.command_schedule_binding.is_none()
                    && scenario.compute_power_binding.is_none(),
                "baseline cannot contain a recovery action"
            );
            None
        }
        Some(SimulationScenarioRole::Recovery) => {
            let action = scenario
                .modeled_action
                .ok_or_else(|| anyhow!("missing modeled action"))?;
            ensure!(
                scenario.has_action_binding(),
                "wrong recovery action binding"
            );
            ensure!(
                scenario.allowed_actions.contains(&action),
                "modeled action is not allowed by scenario"
            );
            if action == AllowedAction::Shutdown {
                ensure!(
                    initialization.compute_power_bindings.iter().any(|b| {
                        Some(b.id.as_str()) == scenario.compute_power_binding.as_deref()
                    }),
                    "unknown executable compute-power binding"
                );
                None
            } else {
                let id = scenario
                    .command_schedule_binding
                    .as_deref()
                    .ok_or_else(|| anyhow!("missing command schedule binding"))?;
                let schedule = initialization
                    .command_schedules
                    .iter()
                    .find(|s| s.id == id)
                    .ok_or_else(|| anyhow!("unknown executable command schedule '{id}'"))?;
                let mode = schedule
                    .action_modes
                    .get(&action)
                    .ok_or_else(|| anyhow!("action has no executable mode mapping"))?;
                ensure!(
                    scenario.allowed_actions.contains(&action),
                    "modeled action is not allowed by scenario"
                );
                Some((id, mode))
            }
        }
        None => bail!("initialized simulation requires an explicit scenario role"),
    };
    for schedule in &initialization.command_schedules {
        // Erase the bundled plan in both runs. Only the selected recovery adds
        // a pointing mode at the common frozen UTC-MJD start epoch.
        let value = match selected {
            Some((id, mode)) if id == schedule.id => {
                format!("[({epoch:?}, {})]", serde_json::to_string(mode)?)
            }
            _ => "[]".into(),
        };
        for engine in &schedule.engines {
            patches.push(EdsPatch::new(
                &schedule.agent_id,
                engine,
                &schedule.field,
                "[(f64, str)]",
                &value,
            ));
        }
    }
    for binding in &initialization.compute_power_bindings {
        let power = if scenario.modeled_action == Some(AllowedAction::Shutdown)
            && scenario.compute_power_binding.as_deref() == Some(binding.id.as_str())
        {
            binding.shutdown_power_w
        } else {
            binding.operating_power_w
        };
        patches.push(EdsPatch::new(
            &binding.agent_id,
            &binding.engine,
            &binding.field,
            "f64",
            &format!("{power:?}"),
        ));
    }
    patches.extend(scenario_patches);
    let mut targets = HashSet::new();
    for patch in &patches {
        ensure!(
            targets.insert((&patch.agent_id, &patch.engine, &patch.field)),
            "scenario patch would overwrite a frozen state or command binding"
        );
    }
    Ok(PreparedInputs {
        epoch_mjd: Some(epoch),
        patches,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AnomalyRecoveryModeConfig;
    use serde_json::json;

    fn shutdown_config() -> AnomalyRecoveryModeConfig {
        serde_json::from_str(include_str!("../testdata/shutdown_profile.json")).unwrap()
    }

    #[test]
    fn shutdown_changes_only_compute_power_and_uses_frozen_state() {
        let config = shutdown_config();
        config.validate().unwrap();
        let simulation = config.simulation.as_ref().unwrap();
        let frame = TelemetrySample {
            source: Some("example".into()),
            ts_mono: 1,
            payload: json!({"telemetry": {"time_mjd_utc": 61290.0, "state_of_charge": 0.8}}),
        };
        let before = prepare_inputs(simulation, &simulation.scenarios[0], &frame, vec![]).unwrap();
        let after = prepare_inputs(simulation, &simulation.scenarios[1], &frame, vec![]).unwrap();
        assert_eq!(before.epoch_mjd, after.epoch_mjd);
        assert_eq!(before.patches.len(), after.patches.len());
        let differences: Vec<_> = before
            .patches
            .iter()
            .zip(&after.patches)
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].0.field, "compute.power");
        assert_eq!(differences[0].0.value, "12.0");
        assert_eq!(differences[0].1.value, "0.5");
        assert!(
            prepare_inputs(
                simulation,
                &simulation.scenarios[1],
                &frame,
                vec![after.patches.last().unwrap().clone()]
            )
            .is_err()
        );
    }

    #[test]
    fn shutdown_contract_rejects_missing_or_ambiguous_and_invalid_bindings() {
        for mutation in 0..9 {
            let mut config = shutdown_config();
            let sim = config.simulation.as_mut().unwrap();
            match mutation {
                0 => sim.initialization = None,
                1 => sim.scenarios[1].compute_power_binding = Some("unknown".into()),
                2 => sim.scenarios[1].command_schedule_binding = Some("pointing".into()),
                3 => {
                    sim.initialization.as_mut().unwrap().compute_power_bindings[0]
                        .shutdown_power_w = -1.0
                }
                4 => {
                    sim.initialization.as_mut().unwrap().compute_power_bindings[0]
                        .shutdown_power_w = 12.0
                }
                5 => {
                    sim.initialization.as_mut().unwrap().compute_power_bindings[0]
                        .operating_power_w = f64::NAN
                }
                6 => {
                    sim.initialization.as_mut().unwrap().compute_power_bindings[0].field =
                        "battery.soc".into()
                }
                7 => sim.scenarios[1].compute_power_binding = None,
                8 => sim.scenarios[0].compute_power_binding = Some("compute_load".into()),
                _ => unreachable!(),
            }
            assert!(config.validate().is_err(), "mutation {mutation}");
        }
        let mut config = shutdown_config();
        config.simulation = None;
        assert!(config.validate().is_err());
    }

    fn pointing_config() -> AnomalyRecoveryModeConfig {
        let config: AnomalyRecoveryModeConfig =
            serde_json::from_str(include_str!("../testdata/pointing_profile.json")).unwrap();
        config.validate().unwrap();
        config
    }

    fn telemetry(config: &AnomalyRecoveryModeConfig) -> TelemetrySample {
        TelemetrySample {
            source: Some(config.nominal_profiles[0].source.clone()),
            ts_mono: 1,
            payload: json!({
                "augmented": {
                    "time_mjd_utc": 61290.0,
                    "state_of_charge": 0.8,
                    "position_eci_km": [0.0, 7000.0, 0.0],
                    "velocity_eci_km_s": [-7.546, 0.0, 0.0],
                    "body_to_eci_quaternion_xyzw": [0.0, 0.0, 0.0, 1.0],
                    "sun_eci_km": [-148000000.0, 30000000.0, 13000000.0],
                    "in_shadow": false
                },
                "sensors": {
                    "gyro_valid": true,
                    "gyro_deg_s": [0.0, 0.0, 0.0],
                    "wheel_rpm": 100.0
                }
            }),
        }
    }

    #[test]
    fn frozen_state_is_identical_and_only_selected_schedule_changes() {
        let config = pointing_config();
        let simulation = config.simulation.as_ref().unwrap();
        let initialization = simulation.initialization.as_ref().unwrap();
        let frame = telemetry(&config);
        let baseline =
            prepare_inputs(simulation, &simulation.scenarios[0], &frame, vec![]).unwrap();
        assert_eq!(baseline.epoch_mjd, Some(61290.0));
        let schedule = &initialization.command_schedules[0];
        for scenario in &simulation.scenarios[1..] {
            let recovery = prepare_inputs(simulation, scenario, &frame, vec![]).unwrap();
            assert_eq!(baseline.epoch_mjd, recovery.epoch_mjd);
            assert_eq!(baseline.patches.len(), recovery.patches.len());
            let mode = &schedule.action_modes[&scenario.modeled_action.unwrap()];
            let mut differences = 0;
            for (before, after) in baseline.patches.iter().zip(&recovery.patches) {
                if before != after {
                    differences += 1;
                    assert_eq!(after.field, schedule.field);
                    assert!(schedule.engines.contains(&after.engine));
                    assert_eq!(before.value, "[]");
                    assert_eq!(
                        after.value,
                        format!("[(61290.0, {})]", serde_json::to_string(mode).unwrap())
                    );
                }
            }
            assert_eq!(differences, schedule.engines.len());
        }
        assert!(
            baseline
                .patches
                .iter()
                .any(|p| p.value == "0.8" && p.engine == "power")
        );
    }

    #[test]
    fn input_requirements_shapes_sources_and_overwrites_are_enforced() {
        let config = pointing_config();
        let simulation = config.simulation.as_ref().unwrap();
        let scenario = &simulation.scenarios[1];
        let original = telemetry(&config);
        let mut wrong_source = original.clone();
        wrong_source.source = Some("other".into());
        assert!(prepare_inputs(simulation, scenario, &wrong_source, vec![]).is_err());
        for (path, replacement) in [
            ("/augmented/time_mjd_utc", Value::Null),
            ("/augmented/position_eci_km", json!([1, 2])),
            ("/augmented/in_shadow", json!(0)),
            ("/sensors/gyro_valid", json!(false)),
            ("/augmented/state_of_charge", json!("0.8")),
            ("/augmented/state_of_charge", json!(1.2)),
        ] {
            let mut invalid = original.clone();
            *invalid.payload.pointer_mut(path).unwrap() = replacement;
            assert!(
                prepare_inputs(simulation, scenario, &invalid, vec![]).is_err(),
                "{path}"
            );
        }
        let prepared = prepare_inputs(simulation, scenario, &original, vec![]).unwrap();
        assert!(
            prepare_inputs(
                simulation,
                scenario,
                &original,
                vec![prepared.patches[0].clone()]
            )
            .is_err()
        );
        let mut missing_binding = scenario.clone();
        missing_binding.command_schedule_binding = Some("unimplemented_label".into());
        assert!(prepare_inputs(simulation, &missing_binding, &original, vec![]).is_err());
        let mut legacy = simulation.clone();
        legacy.initialization = None;
        assert!(prepare_inputs(&legacy, scenario, &original, vec![]).is_err());
    }

    #[test]
    fn configured_vector_and_rate_conversions_are_applied() {
        let config = pointing_config();
        let simulation = config.simulation.as_ref().unwrap();
        let mut frame = telemetry(&config);
        frame.payload["sensors"]["gyro_deg_s"][0] = json!(180.0);
        let input = prepare_inputs(simulation, &simulation.scenarios[0], &frame, vec![]).unwrap();
        let angular = input
            .patches
            .iter()
            .find(|p| p.field == "root!.angular_velocity")
            .unwrap();
        let values: Vec<f64> = serde_json::from_str(&angular.value).unwrap();
        assert!((values[0] - std::f64::consts::PI).abs() < 1e-12);
        let wheel = input
            .patches
            .iter()
            .find(|p| p.engine == "gnc" && p.field.ends_with(".speed"))
            .unwrap();
        assert!(
            (wheel.value.parse::<f64>().unwrap() - 100.0 * std::f64::consts::TAU / 60.0).abs()
                < 1e-12
        );
    }

    #[test]
    fn config_rejects_mismatched_action_binding_and_paired_state() {
        let mut config = pointing_config();
        config.simulation.as_mut().unwrap().scenarios[1].command_schedule_binding =
            Some("label_only".into());
        assert!(config.validate().is_err());
        let mut config = pointing_config();
        config.simulation.as_mut().unwrap().scenarios[1].state_bindings[0].path =
            "different".into();
        assert!(config.validate().is_err());
        let mut config = pointing_config();
        config.simulation.as_mut().unwrap().scenarios[1].duration_days = 1.0;
        assert!(config.validate().is_err());
    }

    #[tokio::test]
    #[ignore = "requires deployment EDS configuration and telemetry; runs real EDS without an LLM"]
    async fn local_eds_executes_distinct_baseline_sun_and_nadir_scenarios() {
        let config_path = std::env::var_os("ANOMALY_RECOVERY_EDS_CONFIG")
            .expect("set ANOMALY_RECOVERY_EDS_CONFIG to a deployment mode_config JSON file");
        let mut config: AnomalyRecoveryModeConfig = serde_json::from_slice(
            &std::fs::read(config_path).expect("read deployment mode configuration"),
        )
        .expect("parse deployment mode configuration");
        if let Some(path) = std::env::var_os("ANOMALY_RECOVERY_EDS_PATH") {
            config.simulation.as_mut().unwrap().eds_path = path.into();
        }
        config.validate().unwrap();
        let simulation = config.simulation.as_ref().unwrap();
        let telemetry_path = std::env::var_os("ANOMALY_RECOVERY_EDS_TELEMETRY").expect(
            "set ANOMALY_RECOVERY_EDS_TELEMETRY to a deployment telemetry payload JSON file",
        );
        let frame = TelemetrySample {
            source: Some(simulation.initialization.as_ref().unwrap().source.clone()),
            ts_mono: 1,
            payload: serde_json::from_slice(
                &std::fs::read(telemetry_path).expect("read telemetry payload"),
            )
            .expect("parse telemetry payload"),
        };
        let schedule = &simulation
            .initialization
            .as_ref()
            .unwrap()
            .command_schedules[0];
        let mode_field = schedule.field.replace(".mode_schedule", ".active_mode");
        let mut observed_modes = Vec::new();
        let mut commanded_attitudes = Vec::new();
        let mut actual_attitudes = Vec::new();
        let mut metrics = Vec::new();
        for scenario in &simulation.scenarios {
            let result =
                crate::planner::collect_scenario(&config, scenario, &frame, &HashMap::new())
                    .await
                    .unwrap();
            assert!(result.success, "{}: {}", scenario.id, result.stderr);
            let extracted = crate::planner::extract_metrics(scenario, &result).unwrap();
            for value in extracted.values() {
                assert!(value.is_finite() && (0.0..=1.0).contains(value));
            }
            let cdh = result
                .frames_by_file
                .iter()
                .find(|(name, _)| name.ends_with(".cdh.jsonl"))
                .unwrap()
                .1;
            let mode = format!(
                "{:?}",
                cdh.last().unwrap().get_by_field(&mode_field).unwrap().data
            );
            let gnc = result
                .frames_by_file
                .iter()
                .find(|(name, _)| name.ends_with(".gnc.jsonl"))
                .unwrap()
                .1;
            let attitude = format!(
                "{:?}",
                gnc.last()
                    .unwrap()
                    .get_by_field("root.commanded_attitude")
                    .unwrap()
                    .data
            );
            let actual = format!(
                "{:?}",
                gnc.last()
                    .unwrap()
                    .get_by_field("root.attitude")
                    .unwrap()
                    .data
            );
            println!(
                "{}: mode={mode}, commanded_attitude={attitude}, actual_attitude={actual}, SOC={extracted:?}",
                scenario.id
            );
            observed_modes.push(mode);
            commanded_attitudes.push(attitude);
            actual_attitudes.push(actual);
            metrics.push(extracted);
        }
        assert_ne!(observed_modes[0], observed_modes[1]);
        assert_ne!(observed_modes[0], observed_modes[2]);
        assert_ne!(observed_modes[1], observed_modes[2]);
        assert_ne!(commanded_attitudes[0], commanded_attitudes[1]);
        assert_ne!(commanded_attitudes[0], commanded_attitudes[2]);
        assert_ne!(commanded_attitudes[1], commanded_attitudes[2]);
        assert_ne!(actual_attitudes[1], actual_attitudes[2]);
        for recovery in &metrics[1..] {
            assert!(recovery["minimum_state_of_charge"] >= 0.2);
            assert!(recovery["final_state_of_charge"] >= 0.2);
            let degradation = (metrics[0]["minimum_state_of_charge"]
                - recovery["minimum_state_of_charge"])
                .max(metrics[0]["final_state_of_charge"] - recovery["final_state_of_charge"]);
            assert!(degradation <= 0.1);
        }
    }
}
