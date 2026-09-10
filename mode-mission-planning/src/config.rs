use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PointingConfig {
    #[default]
    Nadir,
    SunYaw,
    Quaternion {
        x: f64,
        y: f64,
        z: f64,
        w: f64,
    },
    Ypr {
        roll_deg: f64,
        pitch_deg: f64,
        yaw_deg: f64,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetConfig {
    pub(crate) name: String,
    pub(crate) in_fov_field: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GroundStationConfig {
    pub(crate) name: String,
    pub(crate) latitude_deg: f64,
    pub(crate) longitude_deg: f64,
    pub(crate) altitude_m: f64,
    pub(crate) elevation_field: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckAggregation {
    #[default]
    Last,
    Min,
    Max,
    Mean,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ComparisonOp {
    Lt,
    Lte,
    Gt,
    Gte,
    Eq,
    Ne,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FieldCheck {
    pub(crate) target_file: String,
    pub(crate) field: String,
    #[serde(default)]
    pub(crate) aggregation: CheckAggregation,
    pub(crate) op: ComparisonOp,
    pub(crate) threshold: f64,
    #[serde(default = "default_check_tolerance")]
    pub(crate) tolerance: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MissionPlanningConfig {
    pub(crate) eds_path: PathBuf,
    pub(crate) input_adapter_command: Vec<String>,
    #[serde(default)]
    pub(crate) input_adapter_config: serde_json::Value,
    #[serde(default = "default_adapter_timeout_secs")]
    pub(crate) input_adapter_timeout_secs: u64,
    #[serde(default = "default_simulation_timeout_secs")]
    pub(crate) simulation_timeout_secs: u64,
    #[serde(default = "default_planning_horizon_secs")]
    pub(crate) planning_horizon_secs: f64,
    #[serde(default = "default_min_replan_interval_secs")]
    pub(crate) min_replan_interval_secs: u64,
    #[serde(default = "default_command_lead_secs")]
    pub(crate) command_lead_secs: f64,
    #[serde(default = "default_command_dedup_tolerance_secs")]
    pub(crate) command_dedup_tolerance_secs: f64,
    pub(crate) telemetry_gps_time_pointer: String,
    pub(crate) telemetry_state_of_charge_pointer: String,
    pub(crate) result_file: String,
    #[serde(default = "default_time_field")]
    pub(crate) time_field: String,
    pub(crate) state_of_charge_field: String,
    pub(crate) low_power_state_of_charge: f64,
    pub(crate) recovered_state_of_charge: f64,
    pub(crate) minimum_elevation_deg: f64,
    #[serde(default)]
    pub(crate) default_pointing: PointingConfig,
    #[serde(default)]
    pub(crate) targets: Vec<TargetConfig>,
    #[serde(default)]
    pub(crate) ground_stations: Vec<GroundStationConfig>,
    #[serde(default)]
    pub(crate) validation_checks: Vec<FieldCheck>,
}

impl Default for MissionPlanningConfig {
    fn default() -> Self {
        Self {
            eds_path: PathBuf::new(),
            input_adapter_command: Vec::new(),
            input_adapter_config: serde_json::Value::Null,
            input_adapter_timeout_secs: default_adapter_timeout_secs(),
            simulation_timeout_secs: default_simulation_timeout_secs(),
            planning_horizon_secs: default_planning_horizon_secs(),
            min_replan_interval_secs: default_min_replan_interval_secs(),
            command_lead_secs: default_command_lead_secs(),
            command_dedup_tolerance_secs: default_command_dedup_tolerance_secs(),
            telemetry_gps_time_pointer: String::new(),
            telemetry_state_of_charge_pointer: String::new(),
            result_file: String::new(),
            time_field: default_time_field(),
            state_of_charge_field: String::new(),
            low_power_state_of_charge: 0.0,
            recovered_state_of_charge: 1.0,
            minimum_elevation_deg: 0.0,
            default_pointing: PointingConfig::default(),
            targets: Vec::new(),
            ground_stations: Vec::new(),
            validation_checks: Vec::new(),
        }
    }
}

impl MissionPlanningConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.eds_path.as_os_str().is_empty() {
            bail!("eds_path must be configured");
        }
        if self.input_adapter_command.is_empty() || self.input_adapter_command[0].trim().is_empty()
        {
            bail!("input_adapter_command must contain an executable");
        }
        if self.input_adapter_timeout_secs == 0 || self.simulation_timeout_secs == 0 {
            bail!("adapter and simulation timeouts must be greater than zero");
        }
        finite_positive("planning_horizon_secs", self.planning_horizon_secs)?;
        finite_nonnegative("command_lead_secs", self.command_lead_secs)?;
        finite_nonnegative(
            "command_dedup_tolerance_secs",
            self.command_dedup_tolerance_secs,
        )?;
        pointer(
            "telemetry_gps_time_pointer",
            &self.telemetry_gps_time_pointer,
        )?;
        pointer(
            "telemetry_state_of_charge_pointer",
            &self.telemetry_state_of_charge_pointer,
        )?;
        nonempty("result_file", &self.result_file)?;
        nonempty("time_field", &self.time_field)?;
        nonempty("state_of_charge_field", &self.state_of_charge_field)?;
        unit_interval("low_power_state_of_charge", self.low_power_state_of_charge)?;
        unit_interval("recovered_state_of_charge", self.recovered_state_of_charge)?;
        if self.low_power_state_of_charge >= self.recovered_state_of_charge {
            bail!("low_power_state_of_charge must be below recovered_state_of_charge");
        }
        if !self.minimum_elevation_deg.is_finite()
            || !(-90.0..=90.0).contains(&self.minimum_elevation_deg)
        {
            bail!("minimum_elevation_deg must be finite and in [-90, 90]");
        }
        validate_pointing(&self.default_pointing)?;

        let mut names = HashSet::new();
        let mut fields = HashSet::new();
        for target in &self.targets {
            named_field("target", &target.name, &target.in_fov_field)?;
            if !names.insert(target.name.as_str()) {
                bail!("duplicate target name '{}'", target.name);
            }
            if !fields.insert(target.in_fov_field.as_str()) {
                bail!("duplicate target in_fov_field '{}'", target.in_fov_field);
            }
        }
        names.clear();
        fields.clear();
        for station in &self.ground_stations {
            named_field("ground station", &station.name, &station.elevation_field)?;
            if !names.insert(station.name.as_str()) {
                bail!("duplicate ground station name '{}'", station.name);
            }
            if !fields.insert(station.elevation_field.as_str()) {
                bail!(
                    "duplicate ground station elevation_field '{}'",
                    station.elevation_field
                );
            }
            if !station.latitude_deg.is_finite()
                || !(-90.0..=90.0).contains(&station.latitude_deg)
                || !station.longitude_deg.is_finite()
                || !(-180.0..=180.0).contains(&station.longitude_deg)
                || !station.altitude_m.is_finite()
            {
                bail!("ground station '{}' has an invalid location", station.name);
            }
        }
        for check in &self.validation_checks {
            nonempty("validation target_file", &check.target_file)?;
            nonempty("validation field", &check.field)?;
            if !check.threshold.is_finite() || !check.tolerance.is_finite() || check.tolerance < 0.0
            {
                bail!("validation check values must be finite and tolerance non-negative");
            }
        }
        Ok(())
    }
}

fn validate_pointing(pointing: &PointingConfig) -> Result<()> {
    match pointing {
        PointingConfig::Nadir | PointingConfig::SunYaw => Ok(()),
        PointingConfig::Quaternion { x, y, z, w } => {
            if ![x, y, z, w].iter().all(|value| value.is_finite()) {
                bail!("default pointing quaternion must be finite");
            }
            let norm_squared = x * x + y * y + z * z + w * w;
            if norm_squared <= f64::EPSILON {
                bail!("default pointing quaternion must have non-zero norm");
            }
            Ok(())
        }
        PointingConfig::Ypr {
            roll_deg,
            pitch_deg,
            yaw_deg,
        } => {
            if ![roll_deg, pitch_deg, yaw_deg]
                .iter()
                .all(|value| value.is_finite())
            {
                bail!("default pointing YPR values must be finite");
            }
            Ok(())
        }
    }
}

fn named_field(kind: &str, name: &str, field: &str) -> Result<()> {
    nonempty(&format!("{kind} name"), name)?;
    nonempty(&format!("{kind} field"), field)
}

fn pointer(name: &str, value: &str) -> Result<()> {
    if value.is_empty() || !value.starts_with('/') {
        bail!("{name} must be a non-empty JSON Pointer beginning with '/'");
    }
    Ok(())
}

fn nonempty(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{name} must not be empty");
    }
    Ok(())
}

fn finite_positive(name: &str, value: f64) -> Result<()> {
    if !value.is_finite() || value <= 0.0 {
        bail!("{name} must be finite and greater than zero");
    }
    Ok(())
}

fn finite_nonnegative(name: &str, value: f64) -> Result<()> {
    if !value.is_finite() || value < 0.0 {
        bail!("{name} must be finite and non-negative");
    }
    Ok(())
}

fn unit_interval(name: &str, value: f64) -> Result<()> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        bail!("{name} must be finite and in [0, 1]");
    }
    Ok(())
}

fn default_adapter_timeout_secs() -> u64 {
    30
}
fn default_simulation_timeout_secs() -> u64 {
    120
}
fn default_planning_horizon_secs() -> f64 {
    21_600.0
}
fn default_min_replan_interval_secs() -> u64 {
    300
}
fn default_command_lead_secs() -> f64 {
    5.0
}
fn default_command_dedup_tolerance_secs() -> f64 {
    1.0
}
fn default_time_field() -> String {
    "time".to_string()
}
fn default_check_tolerance() -> f64 {
    1.0e-9
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_contain_no_mission_identifiers() {
        let config = MissionPlanningConfig::default();
        assert!(config.eds_path.as_os_str().is_empty());
        assert!(config.result_file.is_empty());
        assert!(config.targets.is_empty());
        assert!(config.ground_stations.is_empty());
    }

    #[test]
    fn hysteresis_thresholds_must_not_overlap() {
        let config = MissionPlanningConfig {
            eds_path: "/tmp/example-eds".into(),
            input_adapter_command: vec!["/tmp/example-adapter".into()],
            telemetry_gps_time_pointer: "/gps_time".into(),
            telemetry_state_of_charge_pointer: "/soc".into(),
            result_file: "result.jsonl".into(),
            state_of_charge_field: "soc".into(),
            low_power_state_of_charge: 0.5,
            recovered_state_of_charge: 0.5,
            ..MissionPlanningConfig::default()
        };
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("must be below")
        );
    }
}
