use anyhow::{Result, bail};
use safe::protocol::{Command, TimedCommand};
use safe::utils::{SECONDS_PER_DAY, utc_mjd_to_gps};

use crate::config::{GroundStationConfig, MissionPlanningConfig, PointingConfig};

#[derive(Debug, Clone)]
pub(crate) struct PlanningSample {
    pub(crate) time_mjd: f64,
    pub(crate) state_of_charge: f64,
    pub(crate) target_visible: bool,
    pub(crate) station_elevations_deg: Vec<f64>,
}

#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub(crate) commands: Vec<TimedCommand>,
    pub(crate) current_power_saving: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointingState {
    Default,
    SunTracking,
    GroundStation(usize),
}

pub(crate) fn build_plan(
    config: &MissionPlanningConfig,
    samples: &[PlanningSample],
    simulation_start_mjd: f64,
    current_gps_time: f64,
    current_state_of_charge: f64,
    prior_power_saving: bool,
) -> Result<Plan> {
    if samples.is_empty() {
        bail!("baseline simulation returned no planning samples");
    }

    let horizon_end_mjd = simulation_start_mjd + config.planning_horizon_secs / SECONDS_PER_DAY;
    let earliest_gps_time = current_gps_time + config.command_lead_secs;
    let Some(earliest_mjd) = safe::utils::gps_to_utc_mjd(earliest_gps_time) else {
        bail!("could not convert earliest command GPS time to MJD");
    };

    let samples = samples
        .iter()
        .filter(|sample| {
            sample.time_mjd >= simulation_start_mjd && sample.time_mjd <= horizon_end_mjd
        })
        .collect::<Vec<_>>();
    if samples.is_empty() {
        bail!("baseline simulation has no samples inside the planning horizon");
    }
    if samples
        .iter()
        .any(|sample| sample.station_elevations_deg.len() != config.ground_stations.len())
    {
        bail!("simulation sample ground-station count does not match configuration");
    }

    let current_power_saving = if current_state_of_charge <= config.low_power_state_of_charge {
        true
    } else if current_state_of_charge >= config.recovered_state_of_charge {
        false
    } else {
        prior_power_saving
    };
    let mut power_saving = current_power_saving;

    let mut power_states = Vec::with_capacity(samples.len());
    let mut station_states = Vec::with_capacity(samples.len());
    for sample in &samples {
        if power_saving {
            if sample.state_of_charge >= config.recovered_state_of_charge {
                power_saving = false;
            }
        } else if sample.state_of_charge <= config.low_power_state_of_charge {
            power_saving = true;
        }
        power_states.push(power_saving);
        station_states.push(
            sample
                .station_elevations_deg
                .iter()
                .position(|elevation| *elevation >= config.minimum_elevation_deg),
        );
    }

    let mut commands = pointing_commands(
        config,
        &samples,
        &power_states,
        &station_states,
        earliest_mjd,
        earliest_gps_time,
    )?;
    commands.extend(capture_commands(
        &samples,
        &power_states,
        &station_states,
        earliest_mjd,
    )?);
    commands.sort_by(|left, right| command_time(left).total_cmp(&command_time(right)));

    Ok(Plan {
        commands,
        current_power_saving,
    })
}

fn pointing_commands(
    config: &MissionPlanningConfig,
    samples: &[&PlanningSample],
    power_states: &[bool],
    station_states: &[Option<usize>],
    earliest_mjd: f64,
    earliest_gps_time: f64,
) -> Result<Vec<TimedCommand>> {
    let first_index = samples
        .iter()
        .rposition(|sample| sample.time_mjd <= earliest_mjd)
        .unwrap_or(0);
    let mut previous = desired_state(power_states[first_index], station_states[first_index]);
    let mut commands = vec![scheduled(
        command_for_state(config, previous),
        earliest_gps_time,
    )];

    for index in (first_index + 1)..samples.len() {
        let next = desired_state(power_states[index], station_states[index]);
        if next == previous {
            continue;
        }

        commands.push(scheduled(
            command_for_state(config, next),
            mjd_to_gps(samples[index].time_mjd)?,
        ));
        previous = next;
    }
    Ok(commands)
}

fn capture_commands(
    samples: &[&PlanningSample],
    power_states: &[bool],
    station_states: &[Option<usize>],
    earliest_mjd: f64,
) -> Result<Vec<TimedCommand>> {
    let mut commands = Vec::new();
    let mut start = None;
    for index in 0..=samples.len() {
        let visible = samples
            .get(index)
            .is_some_and(|sample| sample.target_visible);
        match (start, visible) {
            (None, true) => start = Some(index),
            (Some(window_start), false) => {
                let window_end = index - 1;
                let midpoint = window_start + (window_end - window_start) / 2;
                if samples[midpoint].time_mjd >= earliest_mjd
                    && !power_states[midpoint]
                    && station_states[midpoint].is_none()
                {
                    commands.push(scheduled(
                        Command::CaptureImage,
                        mjd_to_gps(samples[midpoint].time_mjd)?,
                    ));
                }
                start = None;
            }
            _ => {}
        }
    }
    Ok(commands)
}

fn desired_state(power_saving: bool, station: Option<usize>) -> PointingState {
    if power_saving {
        PointingState::SunTracking
    } else if let Some(index) = station {
        PointingState::GroundStation(index)
    } else {
        PointingState::Default
    }
}

fn command_for_state(config: &MissionPlanningConfig, state: PointingState) -> Command {
    match state {
        PointingState::Default => default_pointing_command(&config.default_pointing),
        PointingState::SunTracking => Command::PointSunYaw,
        PointingState::GroundStation(index) => track_command(&config.ground_stations[index]),
    }
}

fn default_pointing_command(pointing: &PointingConfig) -> Command {
    match pointing {
        PointingConfig::Nadir => Command::PointNadir,
        PointingConfig::SunYaw => Command::PointSunYaw,
        PointingConfig::Quaternion { x, y, z, w } => {
            let (roll_deg, pitch_deg, yaw_deg) = quaternion_to_ypr(*x, *y, *z, *w);
            Command::PointYpr {
                roll_deg,
                pitch_deg,
                yaw_deg,
            }
        }
        PointingConfig::Ypr {
            roll_deg,
            pitch_deg,
            yaw_deg,
        } => Command::PointYpr {
            roll_deg: *roll_deg,
            pitch_deg: *pitch_deg,
            yaw_deg: *yaw_deg,
        },
    }
}

fn quaternion_to_ypr(x: f64, y: f64, z: f64, w: f64) -> (f64, f64, f64) {
    let roll = (2.0 * (w * x + y * z)).atan2(1.0 - 2.0 * (x * x + y * y));
    let pitch = (2.0 * (w * y - z * x)).clamp(-1.0, 1.0).asin();
    let yaw = (2.0 * (w * z + x * y)).atan2(1.0 - 2.0 * (y * y + z * z));
    let radians_to_degrees = 180.0 / std::f64::consts::PI;
    (
        roll * radians_to_degrees,
        pitch * radians_to_degrees,
        -yaw * radians_to_degrees,
    )
}

fn track_command(station: &GroundStationConfig) -> Command {
    Command::Track {
        latitude_deg: station.latitude_deg,
        longitude_deg: station.longitude_deg,
        altitude_m: station.altitude_m,
    }
}

fn scheduled(cmd: Command, gps_time: f64) -> TimedCommand {
    TimedCommand::Scheduled { cmd, gps_time }
}

fn mjd_to_gps(mjd: f64) -> Result<f64> {
    utc_mjd_to_gps(mjd).ok_or_else(|| anyhow::anyhow!("could not convert MJD {mjd} to GPS time"))
}

fn command_time(command: &TimedCommand) -> f64 {
    match command {
        TimedCommand::Scheduled { gps_time, .. } => *gps_time,
        TimedCommand::Now(_) => f64::NEG_INFINITY,
        TimedCommand::NOOP => f64::INFINITY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GroundStationConfig, MissionPlanningConfig, PointingConfig, TargetConfig};

    fn config() -> MissionPlanningConfig {
        MissionPlanningConfig {
            planning_horizon_secs: 1_000.0,
            command_lead_secs: 0.0,
            low_power_state_of_charge: 0.3,
            recovered_state_of_charge: 0.6,
            minimum_elevation_deg: 10.0,
            targets: vec![TargetConfig {
                name: "target".into(),
                in_fov_field: "target.visible".into(),
            }],
            ground_stations: vec![GroundStationConfig {
                name: "station".into(),
                latitude_deg: 1.0,
                longitude_deg: 2.0,
                altitude_m: 3.0,
                elevation_field: "station.elevation".into(),
            }],
            ..MissionPlanningConfig::default()
        }
    }

    fn sample(start: f64, seconds: f64, soc: f64, visible: bool, elevation: f64) -> PlanningSample {
        PlanningSample {
            time_mjd: start + seconds / SECONDS_PER_DAY,
            state_of_charge: soc,
            target_visible: visible,
            station_elevations_deg: vec![elevation],
        }
    }

    #[test]
    fn quaternion_default_pointing_is_emitted_as_ypr() {
        assert!(matches!(
            default_pointing_command(&PointingConfig::Quaternion {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            }),
            Command::PointYpr {
                roll_deg: 0.0,
                pitch_deg: 0.0,
                yaw_deg: 0.0,
            }
        ));
    }

    #[test]
    fn quaternion_default_pointing_uses_flight_yaw_sign() {
        let half_turn = (90.0_f64.to_radians() / 2.0).sin();
        assert!(matches!(
            default_pointing_command(&PointingConfig::Quaternion {
                x: 0.0,
                y: 0.0,
                z: half_turn,
                w: half_turn,
            }),
            Command::PointYpr {
                roll_deg,
                pitch_deg,
                yaw_deg,
            } if roll_deg.abs() < 1.0e-12
                && pitch_deg.abs() < 1.0e-12
                && (yaw_deg + 90.0).abs() < 1.0e-12
        ));
    }

    #[test]
    fn power_then_contact_then_default_and_imaging_priority() {
        let start = 60_000.0;
        let current_gps = utc_mjd_to_gps(start).unwrap();
        let samples = vec![
            sample(start, 0.0, 0.2, false, 20.0),
            sample(start, 10.0, 0.4, false, 20.0),
            sample(start, 20.0, 0.6, false, 20.0),
            sample(start, 30.0, 0.7, true, 0.0),
            sample(start, 40.0, 0.7, true, 0.0),
        ];
        let plan = build_plan(&config(), &samples, start, current_gps, 0.2, false).unwrap();

        assert!(matches!(
            plan.commands[0],
            TimedCommand::Scheduled {
                cmd: Command::PointSunYaw,
                ..
            }
        ));
        assert!(plan.commands.iter().any(|command| matches!(
            command,
            TimedCommand::Scheduled {
                cmd: Command::Track { .. },
                ..
            }
        )));
        assert_eq!(
            plan.commands
                .iter()
                .filter(|command| matches!(
                    command,
                    TimedCommand::Scheduled {
                        cmd: Command::CaptureImage,
                        ..
                    }
                ))
                .count(),
            1
        );
    }

    #[test]
    fn contact_restores_pointing_at_first_nonqualifying_sample() {
        let start = 60_000.0;
        let current_gps = utc_mjd_to_gps(start).unwrap();
        let samples = vec![
            sample(start, 0.0, 0.8, false, 20.0),
            sample(start, 10.0, 0.8, false, 20.0),
            sample(start, 20.0, 0.8, false, 0.0),
        ];
        let plan = build_plan(&config(), &samples, start, current_gps, 0.8, false).unwrap();
        let restore_time = plan
            .commands
            .iter()
            .find_map(|command| match command {
                TimedCommand::Scheduled {
                    cmd: Command::PointNadir,
                    gps_time,
                } => Some(*gps_time),
                _ => None,
            })
            .unwrap();
        assert!((restore_time - (current_gps + 20.0)).abs() < 1.0e-3);
    }

    #[test]
    fn hysteresis_retains_prior_state_inside_deadband() {
        let start = 60_000.0;
        let current_gps = utc_mjd_to_gps(start).unwrap();
        let samples = vec![sample(start, 0.0, 0.5, false, 0.0)];
        let plan = build_plan(&config(), &samples, start, current_gps, 0.5, true).unwrap();
        assert!(matches!(
            plan.commands[0],
            TimedCommand::Scheduled {
                cmd: Command::PointSunYaw,
                ..
            }
        ));
        assert!(plan.current_power_saving);
    }

    #[test]
    fn contiguous_visibility_produces_one_midpoint_capture() {
        let start = 60_000.0;
        let current_gps = utc_mjd_to_gps(start).unwrap();
        let samples = vec![
            sample(start, 0.0, 0.8, true, 0.0),
            sample(start, 10.0, 0.8, true, 0.0),
            sample(start, 20.0, 0.8, true, 0.0),
        ];
        let plan = build_plan(&config(), &samples, start, current_gps, 0.8, false).unwrap();
        let capture_time = plan
            .commands
            .iter()
            .find_map(|command| match command {
                TimedCommand::Scheduled {
                    cmd: Command::CaptureImage,
                    gps_time,
                } => Some(*gps_time),
                _ => None,
            })
            .unwrap();
        assert!((capture_time - (current_gps + 10.0)).abs() < 1.0e-3);
    }

    #[test]
    fn contact_at_visibility_midpoint_suppresses_entire_capture() {
        let start = 60_000.0;
        let current_gps = utc_mjd_to_gps(start).unwrap();
        let samples = vec![
            sample(start, 0.0, 0.8, true, 0.0),
            sample(start, 10.0, 0.8, true, 20.0),
            sample(start, 20.0, 0.8, true, 0.0),
        ];
        let plan = build_plan(&config(), &samples, start, current_gps, 0.8, false).unwrap();
        assert!(!plan.commands.iter().any(|command| matches!(
            command,
            TimedCommand::Scheduled {
                cmd: Command::CaptureImage,
                ..
            }
        )));
    }

    #[test]
    fn lead_time_does_not_shift_an_active_window_midpoint() {
        let start = 60_000.0;
        let current_gps = utc_mjd_to_gps(start).unwrap();
        let mut config = config();
        config.command_lead_secs = 15.0;
        let samples = vec![
            sample(start, 0.0, 0.8, true, 0.0),
            sample(start, 10.0, 0.8, true, 0.0),
            sample(start, 20.0, 0.8, true, 0.0),
            sample(start, 30.0, 0.8, true, 0.0),
        ];
        let plan = build_plan(&config, &samples, start, current_gps, 0.8, false).unwrap();
        assert!(!plan.commands.iter().any(|command| matches!(
            command,
            TimedCommand::Scheduled {
                cmd: Command::CaptureImage,
                ..
            }
        )));
    }

    #[test]
    fn threshold_equality_changes_power_state() {
        let start = 60_000.0;
        let current_gps = utc_mjd_to_gps(start).unwrap();
        let samples = vec![
            sample(start, 0.0, 0.3, false, 0.0),
            sample(start, 10.0, 0.6, false, 0.0),
        ];
        let plan = build_plan(&config(), &samples, start, current_gps, 0.3, false).unwrap();
        assert!(plan.current_power_saving);
        assert!(matches!(
            plan.commands[0],
            TimedCommand::Scheduled {
                cmd: Command::PointSunYaw,
                ..
            }
        ));
        assert!(plan.commands.iter().any(|command| matches!(
            command,
            TimedCommand::Scheduled {
                cmd: Command::PointNadir,
                ..
            }
        )));
    }
}
