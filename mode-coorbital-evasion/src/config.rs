use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

const DEFAULT_AGENT_ID: &str = "PTnYWzsc2Nhywc8WVS4blm";
const DEFAULT_FOV_ID: &str = "6XKRM8YlJrrkFh5M8hsH8r";
const NADIR_MODE_ID: &str = "6VPcrRLY3CrmDrNCpxpVTK";
const SUN_YAW_MODE_ID: &str = "6VPctJwTStz3JdspSxMgVz";

/// `[latitude_deg, longitude_deg, altitude_km]`.
pub(crate) type GroundThreatLocation = [f64; 3];
/// `(epoch_mjd, position_km_eci, velocity_km_s_eci)`.
pub(crate) type SpaceThreatEpochState = (f64, [f64; 3], [f64; 3]);

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CoorbitalEvasionModeConfig {
    #[serde(default)]
    pub(crate) eds_path: PathBuf,
    #[serde(default = "default_gnc_time_step_limits")]
    pub(crate) gnc_time_step_limits: (f64, f64),
    #[serde(default = "default_cdh_time_step_limits")]
    pub(crate) cdh_time_step_limits: (f64, f64),
    #[serde(default = "default_power_time_step_limits")]
    pub(crate) power_time_step_limits: (f64, f64),
    /// Planning horizon in days.
    #[serde(default = "default_sim_duration_days")]
    pub(crate) sim_duration_days: f64,
    #[serde(default = "default_simulation_timeout_secs")]
    pub(crate) simulation_timeout_secs: u64,
    #[serde(default = "default_min_replan_interval_secs")]
    pub(crate) min_replan_interval_secs: u64,
    #[serde(default = "default_command_lead_secs")]
    pub(crate) command_lead_secs: f64,
    #[serde(default = "default_agent_id")]
    pub(crate) agent_id: String,
    #[serde(default = "default_field_of_view_id")]
    pub(crate) field_of_view_id: String,
    #[serde(default)]
    pub(crate) threat_ids: Vec<String>,
    #[serde(default)]
    pub(crate) ground_threat_locations: BTreeMap<String, GroundThreatLocation>,
    #[serde(default)]
    pub(crate) space_threat_epoch_states: BTreeMap<String, SpaceThreatEpochState>,
    /// Physical circular FOV half-angle in degrees.
    #[serde(default = "default_fov_half_angle_deg")]
    pub(crate) fov_half_angle_deg: f64,
    /// Additional planning guard angle in degrees.
    #[serde(default = "default_fov_guard_angle_deg")]
    pub(crate) fov_guard_angle_deg: f64,
    #[serde(default = "default_max_slew_rate_rad_s")]
    pub(crate) max_slew_rate_rad_s: f64,
    #[serde(default = "default_command_dedup_angle_rad")]
    pub(crate) command_dedup_angle_rad: f64,
    #[serde(default = "default_command_dedup_time_secs")]
    pub(crate) command_dedup_time_secs: f64,
    #[serde(default = "default_result_engine")]
    pub(crate) result_engine: String,
    #[serde(default = "default_time_field")]
    pub(crate) time_field: String,
    #[serde(default = "default_position_field")]
    pub(crate) position_field: String,
    #[serde(default = "default_velocity_field")]
    pub(crate) velocity_field: String,
    #[serde(default = "default_attitude_field")]
    pub(crate) attitude_field: String,
    #[serde(default = "default_boresight_field")]
    pub(crate) boresight_field: String,
    #[serde(default = "default_relative_position_field")]
    pub(crate) relative_position_field: String,
    #[serde(default = "default_line_of_sight_field")]
    pub(crate) line_of_sight_field: String,
    #[serde(default = "default_in_field_of_view_field")]
    pub(crate) in_field_of_view_field: String,
    #[serde(default = "default_schedule_patch_engine")]
    pub(crate) schedule_patch_engine: String,
    #[serde(default = "default_pointing_mode_schedule_field")]
    pub(crate) pointing_mode_schedule_field: String,
    #[serde(default = "default_pointing_quaternion_schedule_field")]
    pub(crate) pointing_quaternion_schedule_field: String,
    #[serde(default = "default_nadir_mode_id")]
    pub(crate) nadir_mode_id: String,
    #[serde(default = "default_sun_yaw_mode_id")]
    pub(crate) sun_yaw_mode_id: String,
}

impl Default for CoorbitalEvasionModeConfig {
    fn default() -> Self {
        Self {
            eds_path: PathBuf::new(),
            gnc_time_step_limits: default_gnc_time_step_limits(),
            cdh_time_step_limits: default_cdh_time_step_limits(),
            power_time_step_limits: default_power_time_step_limits(),
            sim_duration_days: default_sim_duration_days(),
            simulation_timeout_secs: default_simulation_timeout_secs(),
            min_replan_interval_secs: default_min_replan_interval_secs(),
            command_lead_secs: default_command_lead_secs(),
            agent_id: default_agent_id(),
            field_of_view_id: default_field_of_view_id(),
            threat_ids: Vec::new(),
            ground_threat_locations: BTreeMap::new(),
            space_threat_epoch_states: BTreeMap::new(),
            fov_half_angle_deg: default_fov_half_angle_deg(),
            fov_guard_angle_deg: default_fov_guard_angle_deg(),
            max_slew_rate_rad_s: default_max_slew_rate_rad_s(),
            command_dedup_angle_rad: default_command_dedup_angle_rad(),
            command_dedup_time_secs: default_command_dedup_time_secs(),
            result_engine: default_result_engine(),
            time_field: default_time_field(),
            position_field: default_position_field(),
            velocity_field: default_velocity_field(),
            attitude_field: default_attitude_field(),
            boresight_field: default_boresight_field(),
            relative_position_field: default_relative_position_field(),
            line_of_sight_field: default_line_of_sight_field(),
            in_field_of_view_field: default_in_field_of_view_field(),
            schedule_patch_engine: default_schedule_patch_engine(),
            pointing_mode_schedule_field: default_pointing_mode_schedule_field(),
            pointing_quaternion_schedule_field: default_pointing_quaternion_schedule_field(),
            nadir_mode_id: default_nadir_mode_id(),
            sun_yaw_mode_id: default_sun_yaw_mode_id(),
        }
    }
}

fn default_gnc_time_step_limits() -> (f64, f64) {
    (0.01, 3.0)
}
fn default_cdh_time_step_limits() -> (f64, f64) {
    (0.1, 60.0)
}
fn default_power_time_step_limits() -> (f64, f64) {
    (0.1, 60.0)
}
fn default_sim_duration_days() -> f64 {
    0.05
}
fn default_simulation_timeout_secs() -> u64 {
    60
}
fn default_min_replan_interval_secs() -> u64 {
    60
}
fn default_command_lead_secs() -> f64 {
    5.0
}
fn default_agent_id() -> String {
    DEFAULT_AGENT_ID.to_string()
}
fn default_field_of_view_id() -> String {
    DEFAULT_FOV_ID.to_string()
}
fn default_fov_half_angle_deg() -> f64 {
    30.0
}
fn default_fov_guard_angle_deg() -> f64 {
    1.0
}
fn default_max_slew_rate_rad_s() -> f64 {
    0.010_472
}
fn default_command_dedup_angle_rad() -> f64 {
    1.0e-6
}
fn default_command_dedup_time_secs() -> f64 {
    1.0
}
fn default_result_engine() -> String {
    "gnc".to_string()
}
fn default_time_field() -> String {
    "time".to_string()
}
fn default_position_field() -> String {
    "root.position".to_string()
}
fn default_velocity_field() -> String {
    "root.velocity".to_string()
}
fn default_attitude_field() -> String {
    "root.attitude".to_string()
}
fn default_boresight_field() -> String {
    "boresight_eci".to_string()
}
fn default_relative_position_field() -> String {
    "relative_position_eci".to_string()
}
fn default_line_of_sight_field() -> String {
    "line_of_sight".to_string()
}
fn default_in_field_of_view_field() -> String {
    "in_field_of_view".to_string()
}
fn default_schedule_patch_engine() -> String {
    "cdh".to_string()
}
fn default_pointing_mode_schedule_field() -> String {
    "6VPcwrnbQS6HBHdy3kWtDC.mode_schedule".to_string()
}
fn default_pointing_quaternion_schedule_field() -> String {
    "6VPcwrnbQS6HBHdy3kWtDC.quaternion_schedule".to_string()
}
fn default_nadir_mode_id() -> String {
    NADIR_MODE_ID.to_string()
}
fn default_sun_yaw_mode_id() -> String {
    SUN_YAW_MODE_ID.to_string()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redesigned_defaults_use_separate_pointing_schedule_fields() {
        let config = CoorbitalEvasionModeConfig::default();
        assert_eq!(config.fov_half_angle_deg, 30.0);
        assert_eq!(config.fov_guard_angle_deg, 1.0);
        assert_eq!(config.max_slew_rate_rad_s, 0.010_472);
        assert_eq!(config.position_field, "root.position");
        assert_eq!(config.velocity_field, "root.velocity");
        assert_eq!(config.attitude_field, "root.attitude");
        assert_eq!(
            config.pointing_mode_schedule_field,
            "6VPcwrnbQS6HBHdy3kWtDC.mode_schedule"
        );
        assert_eq!(
            config.pointing_quaternion_schedule_field,
            "6VPcwrnbQS6HBHdy3kWtDC.quaternion_schedule"
        );
    }
}
