use std::path::PathBuf;

use safe::protocol::AutonomyModeId;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CoorbitalEvasionModeConfig {
    #[serde(default)]
    pub(crate) eds_path: PathBuf,
    /// One-shot program that converts deployment telemetry into EDS patches.
    #[serde(default)]
    pub(crate) input_adapter_command: Vec<String>,
    /// Opaque JSON forwarded unchanged to the input adapter.
    #[serde(default)]
    pub(crate) input_adapter_config: serde_json::Value,
    #[serde(default = "default_adapter_timeout_secs")]
    pub(crate) input_adapter_timeout_secs: u64,
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
    /// Defer planning until proposals from this mode have reached a terminal board state.
    #[serde(default)]
    pub(crate) wait_for_proposals_from_mode_id: Option<AutonomyModeId>,
    /// Fixed allowlist of EDS threat entities to plan against.
    #[serde(default)]
    pub(crate) threat_ids: Vec<String>,
    #[serde(default = "default_agent_id")]
    pub(crate) agent_id: String,
    #[serde(default = "default_field_of_view_id")]
    pub(crate) field_of_view_id: String,
    /// Maximum threat range to consider, in kilometers.
    #[serde(default = "default_threat_max_range_km")]
    pub(crate) threat_max_range_km: f64,
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
    #[serde(default = "default_pointing_rpy_schedule_field")]
    pub(crate) pointing_rpy_schedule_field: String,
    #[serde(default = "default_nadir_mode_id")]
    pub(crate) nadir_mode_id: String,
    #[serde(default = "default_sun_yaw_mode_id")]
    pub(crate) sun_yaw_mode_id: String,
}

impl Default for CoorbitalEvasionModeConfig {
    fn default() -> Self {
        Self {
            eds_path: PathBuf::new(),
            input_adapter_command: Vec::new(),
            input_adapter_config: serde_json::Value::Null,
            input_adapter_timeout_secs: default_adapter_timeout_secs(),
            gnc_time_step_limits: default_gnc_time_step_limits(),
            cdh_time_step_limits: default_cdh_time_step_limits(),
            power_time_step_limits: default_power_time_step_limits(),
            sim_duration_days: default_sim_duration_days(),
            simulation_timeout_secs: default_simulation_timeout_secs(),
            min_replan_interval_secs: default_min_replan_interval_secs(),
            command_lead_secs: default_command_lead_secs(),
            wait_for_proposals_from_mode_id: None,
            threat_ids: Vec::new(),
            agent_id: default_agent_id(),
            field_of_view_id: default_field_of_view_id(),
            threat_max_range_km: default_threat_max_range_km(),
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
            pointing_rpy_schedule_field: default_pointing_rpy_schedule_field(),
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
fn default_adapter_timeout_secs() -> u64 {
    30
}
fn default_min_replan_interval_secs() -> u64 {
    60
}
fn default_command_lead_secs() -> f64 {
    5.0
}
fn default_agent_id() -> String {
    String::new()
}
fn default_field_of_view_id() -> String {
    String::new()
}
fn default_threat_max_range_km() -> f64 {
    f64::INFINITY
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
    String::new()
}
fn default_pointing_rpy_schedule_field() -> String {
    String::new()
}
fn default_nadir_mode_id() -> String {
    String::new()
}
fn default_sun_yaw_mode_id() -> String {
    String::new()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_contain_no_deployment_identifiers() {
        let config = CoorbitalEvasionModeConfig::default();
        assert_eq!(config.fov_half_angle_deg, 30.0);
        assert_eq!(config.fov_guard_angle_deg, 1.0);
        assert!(config.threat_max_range_km.is_infinite());
        assert_eq!(config.max_slew_rate_rad_s, 0.010_472);
        assert!(config.wait_for_proposals_from_mode_id.is_none());
        assert!(config.threat_ids.is_empty());
        assert!(config.agent_id.is_empty());
        assert!(config.field_of_view_id.is_empty());
        assert!(config.pointing_mode_schedule_field.is_empty());
        assert!(config.pointing_rpy_schedule_field.is_empty());
        assert!(config.nadir_mode_id.is_empty());
        assert!(config.sun_yaw_mode_id.is_empty());
    }

    #[test]
    fn omitted_threat_max_range_preserves_unlimited_range() {
        let config: CoorbitalEvasionModeConfig = serde_json::from_str("{}").unwrap();
        assert!(config.threat_max_range_km.is_infinite());
    }
}
