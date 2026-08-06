use std::time::Instant;

use nalgebra::{UnitQuaternion, Vector3};
use safe::protocol::AutonomyModeBoardState;
use safe_telemetry::model::Telemetry;

use crate::config::ElectronicWarfareModeConfig;

#[derive(Debug, Clone)]
pub(crate) struct ThreatGeometry {
    pub(crate) relative_position_eci: Vector3<f64>,
    pub(crate) line_of_sight: bool,
    pub(crate) in_field_of_view: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct GeometrySample {
    pub(crate) time_mjd: f64,
    pub(crate) position_eci: Vector3<f64>,
    pub(crate) velocity_eci: Vector3<f64>,
    pub(crate) attitude_body_to_eci: UnitQuaternion<f64>,
    pub(crate) boresight_eci: Vector3<f64>,
    pub(crate) threats: Vec<ThreatGeometry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnsafePeriod {
    pub(crate) first: usize,
    pub(crate) last: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ModeledTarget {
    Nadir,
    Fixed(Vector3<f64>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ModelCommand {
    pub(crate) sample_index: usize,
    pub(crate) target: ModeledTarget,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct ScheduleScore {
    pub(crate) exposure_secs: f64,
    pub(crate) nadir_cost: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PointingTarget {
    Nadir,
    Quaternion(UnitQuaternion<f64>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScheduledPointing {
    pub(crate) time_mjd: f64,
    pub(crate) target: PointingTarget,
}

pub(crate) type ModeScheduleEntry = (f64, String);
pub(crate) type QuaternionScheduleEntry = (f64, (f64, f64, f64, f64));

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct EdsPointingSchedule {
    pub(crate) mode_schedule: Vec<ModeScheduleEntry>,
    pub(crate) quaternion_schedule: Vec<QuaternionScheduleEntry>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ValidationReport {
    pub(crate) score: ScheduleScore,
    pub(crate) exposed_threat_ids: Vec<String>,
    pub(crate) first_exposed_mjd: Option<f64>,
    pub(crate) last_exposed_mjd: Option<f64>,
}

#[derive(Debug, Clone)]
pub(crate) struct ElectronicWarfarePlan {
    pub(crate) earliest_command_mjd: f64,
    pub(crate) horizon_end_mjd: f64,
    pub(crate) commands: Vec<ScheduledPointing>,
    pub(crate) modeled_score: ScheduleScore,
    pub(crate) validation: ValidationReport,
}

#[derive(Debug, Clone)]
pub(crate) enum PlanningOutcome {
    NoBoardChange,
    Schedule(ElectronicWarfarePlan),
}

pub(crate) struct ElectronicWarfareMode {
    pub(crate) config: ElectronicWarfareModeConfig,
    pub(crate) latest_telemetry: Option<Telemetry>,
    pub(crate) latest_board_snapshot: AutonomyModeBoardState,
    pub(crate) has_board_snapshot: bool,
    pub(crate) last_replan_start: Option<Instant>,
    pub(crate) warned_missing_config: bool,
}

impl ElectronicWarfareMode {
    pub(crate) fn new() -> Self {
        Self {
            config: ElectronicWarfareModeConfig::default(),
            latest_telemetry: None,
            latest_board_snapshot: AutonomyModeBoardState::default(),
            has_board_snapshot: false,
            last_replan_start: None,
            warned_missing_config: false,
        }
    }
}
