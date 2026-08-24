use std::cmp::Ordering;

use anyhow::Context;
use nalgebra::{Matrix3, Quaternion, Rotation3, Unit, UnitQuaternion, Vector3};
use safe::utils::{SECONDS_PER_DAY, gps_to_utc_mjd};
use safe_telemetry::model::Telemetry;

use crate::types::{
    CoorbitalEvasionMode, CoorbitalEvasionPlan, GeometrySample, ModelCommand, ModeledTarget,
    PlanningOutcome, PointingTarget, ScheduleScore, ScheduledPointing, UnsafePeriod,
};

const ANGLE_EPSILON: f64 = 1.0e-12;

pub(crate) fn nadir_direction(sample: &GeometrySample) -> Vector3<f64> {
    -sample.position_eci.normalize()
}

/// The EDS reports threat-to-spacecraft, so negate it to obtain spacecraft-to-threat LOS.
pub(crate) fn threat_los_direction(relative_position_eci: &Vector3<f64>) -> Vector3<f64> {
    -relative_position_eci.normalize()
}

fn accepted_schedule_exposed(samples: &[GeometrySample]) -> bool {
    samples.iter().any(|sample| {
        sample
            .threats
            .iter()
            .any(|threat| threat.line_of_sight && threat.in_field_of_view)
    })
}

fn angle_between(a: &Vector3<f64>, b: &Vector3<f64>) -> f64 {
    a.dot(b).clamp(-1.0, 1.0).acos()
}

fn score_is_better(left: ScheduleScore, right: ScheduleScore) -> bool {
    left.exposure_secs.total_cmp(&right.exposure_secs) == Ordering::Less
        || (left.exposure_secs.total_cmp(&right.exposure_secs) == Ordering::Equal
            && left.nadir_cost.total_cmp(&right.nadir_cost) == Ordering::Less)
}

pub(crate) fn unsafe_periods(
    samples: &[GeometrySample],
    guarded_half_angle_rad: f64,
) -> Vec<UnsafePeriod> {
    let cap_cos = guarded_half_angle_rad.cos();
    let mut periods = Vec::new();
    let mut first = None;

    for (index, sample) in samples.iter().enumerate() {
        let nadir = nadir_direction(sample);
        let unsafe_now = sample.threats.iter().any(|threat| {
            threat.line_of_sight
                && nadir.dot(&threat_los_direction(&threat.relative_position_eci)) >= cap_cos
        });
        match (first, unsafe_now) {
            (None, true) => first = Some(index),
            (Some(start), false) => {
                periods.push(UnsafePeriod {
                    first: start,
                    last: index - 1,
                });
                first = None;
            }
            _ => {}
        }
    }
    if let Some(start) = first {
        periods.push(UnsafePeriod {
            first: start,
            last: samples.len() - 1,
        });
    }
    periods
}

fn orthogonal_unit(direction: &Vector3<f64>) -> Vector3<f64> {
    let reference =
        if direction.x.abs() <= direction.y.abs() && direction.x.abs() <= direction.z.abs() {
            Vector3::x()
        } else if direction.y.abs() <= direction.z.abs() {
            Vector3::y()
        } else {
            Vector3::z()
        };
    direction.cross(&reference).normalize()
}

fn project_to_cap_boundary(
    direction: Vector3<f64>,
    cap_center: Vector3<f64>,
    cap_cos: f64,
    cap_sin: f64,
) -> Vector3<f64> {
    let tangent = direction - direction.dot(&cap_center) * cap_center;
    let tangent = if tangent.norm_squared() <= ANGLE_EPSILON {
        orthogonal_unit(&cap_center)
    } else {
        tangent.normalize()
    };
    (cap_cos * cap_center + cap_sin * tangent).normalize()
}

fn fixed_direction_score(
    samples: &[GeometrySample],
    first: usize,
    last: usize,
    direction: Vector3<f64>,
    fov_half_angle_rad: f64,
) -> ScheduleScore {
    let mut score = ScheduleScore::default();
    let end_interval = (last + 1).min(samples.len().saturating_sub(1));
    let cap_cos = fov_half_angle_rad.cos();
    for index in first..end_interval {
        let dt_secs = (samples[index + 1].time_mjd - samples[index].time_mjd) * SECONDS_PER_DAY;
        let exposed = samples[index].threats.iter().any(|threat| {
            threat.line_of_sight
                && direction.dot(&threat_los_direction(&threat.relative_position_eci)) >= cap_cos
        });
        if exposed {
            score.exposure_secs += dt_secs;
        }
        score.nadir_cost += dt_secs * (1.0 - direction.dot(&nadir_direction(&samples[index])));
    }
    score
}

fn fixed_direction_is_safe(
    samples: &[GeometrySample],
    first: usize,
    last: usize,
    direction: Vector3<f64>,
    guarded_half_angle_rad: f64,
) -> bool {
    let cap_cos = guarded_half_angle_rad.cos();
    samples[first..=last].iter().all(|sample| {
        sample.threats.iter().all(|threat| {
            !threat.line_of_sight
                || direction.dot(&threat_los_direction(&threat.relative_position_eci))
                    < cap_cos + 1.0e-10
        })
    })
}

fn aggregate_nadir_direction(
    samples: &[GeometrySample],
    first: usize,
    last: usize,
) -> Vector3<f64> {
    let mut aggregate = Vector3::zeros();
    for index in first..=last.min(samples.len().saturating_sub(2)) {
        let dt_secs = (samples[index + 1].time_mjd - samples[index].time_mjd) * SECONDS_PER_DAY;
        if dt_secs > 0.0 {
            aggregate += dt_secs * nadir_direction(&samples[index]);
        }
    }
    if aggregate.norm_squared() <= ANGLE_EPSILON {
        nadir_direction(&samples[first])
    } else {
        aggregate.normalize()
    }
}

/// Finds one fixed boresight for a complete threat window.
///
/// The desired direction is the duration-weighted Nadir direction. When that
/// direction violates a threat cap, project it to the most violated cap and
/// repeat. Every proposed direction is checked against every threat sample;
/// the bounded projection is only used to avoid the old candidate/start-time
/// Cartesian search.
fn fixed_window_boresight(
    samples: &[GeometrySample],
    period: UnsafePeriod,
    earliest: usize,
    guarded_half_angle_rad: f64,
    fov_half_angle_rad: f64,
) -> Vector3<f64> {
    const MAX_PROJECTION_ITERATIONS: usize = 128;
    let first = period.first.max(earliest);
    let last = period.last;
    let cap_cos = guarded_half_angle_rad.cos();
    let cap_sin = guarded_half_angle_rad.sin();
    let mut direction = aggregate_nadir_direction(samples, first, last);
    let mut best_direction = direction;
    let mut best_score = fixed_direction_score(samples, first, last, direction, fov_half_angle_rad);

    for _ in 0..MAX_PROJECTION_ITERATIONS {
        let mut worst: Option<(f64, Vector3<f64>)> = None;
        for sample in &samples[first..=last] {
            for threat in &sample.threats {
                if !threat.line_of_sight {
                    continue;
                }
                let threat_direction = threat_los_direction(&threat.relative_position_eci);
                let violation = direction.dot(&threat_direction) - cap_cos;
                if worst
                    .as_ref()
                    .is_none_or(|(worst_violation, _)| violation > *worst_violation)
                {
                    worst = Some((violation, threat_direction));
                }
            }
        }

        let Some((violation, threat_direction)) = worst else {
            break;
        };
        if violation <= 1.0e-10 {
            return direction;
        }

        let score = fixed_direction_score(samples, first, last, direction, fov_half_angle_rad);
        if score_is_better(score, best_score) {
            best_direction = direction;
            best_score = score;
        }
        direction = project_to_cap_boundary(direction, threat_direction, cap_cos, cap_sin);
    }

    let final_score = fixed_direction_score(samples, first, last, direction, fov_half_angle_rad);
    if score_is_better(final_score, best_score) {
        best_direction = direction;
    }
    if fixed_direction_is_safe(samples, first, last, direction, guarded_half_angle_rad) {
        direction
    } else {
        best_direction
    }
}

pub(crate) fn slew_step(
    current: Vector3<f64>,
    target: Vector3<f64>,
    max_angle_rad: f64,
) -> Vector3<f64> {
    let angle = angle_between(&current, &target);
    if angle <= max_angle_rad || angle <= ANGLE_EPSILON {
        return target;
    }
    let mut axis = current.cross(&target);
    if axis.norm_squared() <= ANGLE_EPSILON {
        axis = current.cross(&Vector3::x());
        if axis.norm_squared() <= ANGLE_EPSILON {
            axis = current.cross(&Vector3::y());
        }
    }
    (UnitQuaternion::from_axis_angle(&Unit::new_normalize(axis), max_angle_rad) * current)
        .normalize()
}

fn target_direction(target: ModeledTarget, sample: &GeometrySample) -> Vector3<f64> {
    match target {
        ModeledTarget::Nadir => nadir_direction(sample),
        ModeledTarget::Fixed(direction) => direction,
    }
}

fn modeled_boresights(
    samples: &[GeometrySample],
    earliest: usize,
    commands: &[ModelCommand],
    max_slew_rate_rad_s: f64,
) -> Vec<Vector3<f64>> {
    let mut boresights = samples
        .iter()
        .map(|sample| sample.boresight_eci.normalize())
        .collect::<Vec<_>>();
    let mut ordered = commands.to_vec();
    ordered.sort_by_key(|command| command.sample_index);
    let mut active = ModeledTarget::Nadir;
    let mut next_command = 0;
    while next_command < ordered.len() && ordered[next_command].sample_index <= earliest {
        active = ordered[next_command].target;
        next_command += 1;
    }

    for index in earliest..samples.len().saturating_sub(1) {
        while next_command < ordered.len() && ordered[next_command].sample_index <= index {
            active = ordered[next_command].target;
            next_command += 1;
        }
        let dt_secs = (samples[index + 1].time_mjd - samples[index].time_mjd) * SECONDS_PER_DAY;
        let desired = target_direction(active, &samples[index + 1]);
        boresights[index + 1] =
            slew_step(boresights[index], desired, max_slew_rate_rad_s * dt_secs);
    }
    boresights
}

pub(crate) fn score_boresights(
    samples: &[GeometrySample],
    boresights: &[Vector3<f64>],
    first_interval: usize,
    end_interval: usize,
    fov_half_angle_rad: f64,
) -> ScheduleScore {
    let cap_cos = fov_half_angle_rad.cos();
    let mut score = ScheduleScore::default();
    for index in first_interval..end_interval.min(samples.len().saturating_sub(1)) {
        let dt_secs = (samples[index + 1].time_mjd - samples[index].time_mjd) * SECONDS_PER_DAY;
        let exposed = samples[index].threats.iter().any(|threat| {
            threat.line_of_sight
                && boresights[index].dot(&threat_los_direction(&threat.relative_position_eci))
                    >= cap_cos
        });
        if exposed {
            score.exposure_secs += dt_secs;
        }
        score.nadir_cost +=
            dt_secs * (1.0 - boresights[index].dot(&nadir_direction(&samples[index])));
    }
    score
}

fn command_key_cmp(lhs: &[ModelCommand], rhs: &[ModelCommand]) -> Ordering {
    for (left, right) in lhs.iter().zip(rhs) {
        let ordering = right.sample_index.cmp(&left.sample_index).then_with(|| {
            match (left.target, right.target) {
                (ModeledTarget::Nadir, ModeledTarget::Nadir) => Ordering::Equal,
                (ModeledTarget::Nadir, ModeledTarget::Fixed(_)) => Ordering::Less,
                (ModeledTarget::Fixed(_), ModeledTarget::Nadir) => Ordering::Greater,
                (ModeledTarget::Fixed(a), ModeledTarget::Fixed(b)) => {
                    a.x.total_cmp(&b.x)
                        .then(a.y.total_cmp(&b.y))
                        .then(a.z.total_cmp(&b.z))
                }
            }
        });
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    lhs.len().cmp(&rhs.len())
}

fn compare_schedules(
    left_score: ScheduleScore,
    left: &[ModelCommand],
    right_score: ScheduleScore,
    right: &[ModelCommand],
) -> Ordering {
    left_score
        .exposure_secs
        .total_cmp(&right_score.exposure_secs)
        .then(left_score.nadir_cost.total_cmp(&right_score.nadir_cost))
        .then(left.len().cmp(&right.len()))
        .then_with(|| command_key_cmp(left, right))
}

fn canonicalize_model_commands(mut commands: Vec<ModelCommand>) -> Vec<ModelCommand> {
    commands.sort_by_key(|command| command.sample_index);
    let mut result: Vec<ModelCommand> = Vec::new();
    for command in commands {
        if result
            .last()
            .is_some_and(|previous| previous.sample_index == command.sample_index)
        {
            *result.last_mut().unwrap() = command;
        } else {
            result.push(command);
        }
    }
    result
}

fn sample_dt_secs(samples: &[GeometrySample], index: usize) -> f64 {
    if index + 1 < samples.len() {
        (samples[index + 1].time_mjd - samples[index].time_mjd) * SECONDS_PER_DAY
    } else if index > 0 {
        (samples[index].time_mjd - samples[index - 1].time_mjd) * SECONDS_PER_DAY
    } else {
        0.0
    }
}

fn latest_reachable_start(
    samples: &[GeometrySample],
    earliest: usize,
    start_floor: usize,
    first: usize,
    current: &[ModelCommand],
    target: Vector3<f64>,
    max_slew_rate_rad_s: f64,
) -> usize {
    let planning_samples = &samples[..=first];
    let boresights = modeled_boresights(planning_samples, earliest, current, max_slew_rate_rad_s);
    let required_angle = angle_between(&boresights[first], &target);
    let lower_bound = start_floor.max(earliest);
    if required_angle <= ANGLE_EPSILON {
        return first.max(lower_bound);
    }
    let mut available_secs = 0.0;
    let mut start = lower_bound;
    for index in (lower_bound..first).rev() {
        available_secs += sample_dt_secs(samples, index);
        if max_slew_rate_rad_s * available_secs >= required_angle {
            start = index;
            break;
        }
    }
    start.max(start_floor).max(earliest)
}

fn add_fixed_target(
    samples: &[GeometrySample],
    earliest: usize,
    start_floor: usize,
    period: UnsafePeriod,
    current: &[ModelCommand],
    target: Vector3<f64>,
    max_slew_rate_rad_s: f64,
) -> Vec<ModelCommand> {
    let first = period.first.max(earliest);
    let start = latest_reachable_start(
        samples,
        earliest,
        start_floor,
        first,
        current,
        target,
        max_slew_rate_rad_s,
    );
    let mut commands = current.to_vec();
    commands.push(ModelCommand {
        sample_index: start,
        target: ModeledTarget::Fixed(target),
    });
    canonicalize_model_commands(commands)
}

fn score_commands_through(
    samples: &[GeometrySample],
    earliest: usize,
    end_interval: usize,
    commands: &[ModelCommand],
    max_slew_rate_rad_s: f64,
    fov_half_angle_rad: f64,
) -> ScheduleScore {
    let end_interval = end_interval.min(samples.len().saturating_sub(1));
    let planning_samples = &samples[..=end_interval];
    let boresights = modeled_boresights(planning_samples, earliest, commands, max_slew_rate_rad_s);
    score_boresights(
        planning_samples,
        &boresights,
        earliest,
        end_interval,
        fov_half_angle_rad,
    )
}

fn choose_route(
    samples: &[GeometrySample],
    earliest: usize,
    end_interval: usize,
    left: Vec<ModelCommand>,
    right: Vec<ModelCommand>,
    max_slew_rate_rad_s: f64,
    fov_half_angle_rad: f64,
) -> Vec<ModelCommand> {
    let left_score = score_commands_through(
        samples,
        earliest,
        end_interval,
        &left,
        max_slew_rate_rad_s,
        fov_half_angle_rad,
    );
    let right_score = score_commands_through(
        samples,
        earliest,
        end_interval,
        &right,
        max_slew_rate_rad_s,
        fov_half_angle_rad,
    );
    if compare_schedules(left_score, &left, right_score, &right) != Ordering::Greater {
        left
    } else {
        right
    }
}

fn nadir_reference(sample: &GeometrySample) -> UnitQuaternion<f64> {
    let z_axis = nadir_direction(sample);
    let projected_velocity = sample.velocity_eci - sample.velocity_eci.dot(&z_axis) * z_axis;
    let x_axis = projected_velocity.normalize();
    let y_axis = z_axis.cross(&x_axis).normalize();
    UnitQuaternion::from_rotation_matrix(&Rotation3::from_matrix_unchecked(Matrix3::from_columns(
        &[x_axis, y_axis, z_axis],
    )))
}

pub(crate) fn lift_boresight(
    reference: &UnitQuaternion<f64>,
    target_boresight: Vector3<f64>,
) -> UnitQuaternion<f64> {
    let reference_boresight = reference * Vector3::z();
    let delta = UnitQuaternion::rotation_between(&reference_boresight, &target_boresight)
        .unwrap_or_else(|| {
            let mut axis = reference_boresight.cross(&Vector3::x());
            if axis.norm_squared() <= ANGLE_EPSILON {
                axis = reference_boresight.cross(&Vector3::y());
            }
            UnitQuaternion::from_axis_angle(&Unit::new_normalize(axis), std::f64::consts::PI)
        });
    let lifted = delta * reference;
    let dot = lifted
        .quaternion()
        .coords
        .dot(&reference.quaternion().coords);
    if dot >= 0.0 {
        lifted
    } else {
        UnitQuaternion::new_unchecked(Quaternion::from(-lifted.into_inner().coords))
    }
}

fn lift_commands(samples: &[GeometrySample], commands: &[ModelCommand]) -> Vec<ScheduledPointing> {
    let mut result = Vec::with_capacity(commands.len());
    let mut previous_quaternion = None;
    let mut previous_was_nadir = false;
    for command in commands {
        let target = match command.target {
            ModeledTarget::Nadir => {
                previous_quaternion = None;
                previous_was_nadir = true;
                PointingTarget::Nadir
            }
            ModeledTarget::Fixed(boresight) => {
                let reference = if previous_was_nadir {
                    nadir_reference(&samples[command.sample_index])
                } else {
                    previous_quaternion
                        .unwrap_or(samples[command.sample_index].attitude_body_to_eci)
                };
                let quaternion = lift_boresight(&reference, boresight);
                previous_quaternion = Some(quaternion);
                previous_was_nadir = false;
                PointingTarget::Quaternion(quaternion)
            }
        };
        result.push(ScheduledPointing {
            time_mjd: samples[command.sample_index].time_mjd,
            target,
        });
    }
    result
}

impl CoorbitalEvasionMode {
    fn validate_config(&self) -> anyhow::Result<()> {
        if self.config.threat_ids.is_empty() {
            anyhow::bail!("mode_config.threat_ids must contain at least one threat ID");
        }
        if self.config.sim_duration_days <= 0.0 {
            anyhow::bail!("mode_config.sim_duration_days must be greater than zero");
        }
        if self.config.max_slew_rate_rad_s <= 0.0 {
            anyhow::bail!("mode_config.max_slew_rate_rad_s must be greater than zero");
        }
        Ok(())
    }

    pub(crate) async fn build_plan(
        &self,
        telemetry: &Telemetry,
    ) -> anyhow::Result<PlanningOutcome> {
        self.validate_config()?;
        let sim_start_mjd = gps_to_utc_mjd(telemetry.onboard_time_ms as f64 / 1_000.0)
            .context("could not convert telemetry onboard time to simulation epoch")?;
        let horizon_end_mjd = sim_start_mjd + self.config.sim_duration_days;
        let command_time_mjd = sim_start_mjd + self.config.command_lead_secs / SECONDS_PER_DAY;
        if command_time_mjd >= horizon_end_mjd {
            anyhow::bail!("command_lead_secs must be shorter than sim_duration_days");
        }

        let accepted_schedule = self.accepted_pointing_schedule(sim_start_mjd, horizon_end_mjd);
        let baseline = self
            .run_geometry_simulation(telemetry, &accepted_schedule)
            .await
            .context("failed to simulate accepted pointing schedule")?;
        let earliest = baseline
            .iter()
            .position(|sample| sample.time_mjd >= command_time_mjd)
            .context("simulation has no sample at or after the earliest commandable time")?;
        if !accepted_schedule_exposed(&baseline[earliest..]) {
            return Ok(PlanningOutcome::NoBoardChange);
        }
        let fov_half_angle_rad = self.config.fov_half_angle_deg.to_radians();
        let guarded_half_angle_rad =
            (self.config.fov_half_angle_deg + self.config.fov_guard_angle_deg).to_radians();
        let periods = unsafe_periods(&baseline, guarded_half_angle_rad)
            .into_iter()
            .filter(|period| period.last >= earliest)
            .collect::<Vec<_>>();
        let mut commands = vec![ModelCommand {
            sample_index: earliest,
            target: ModeledTarget::Nadir,
        }];

        let mut previous_period = None;
        for period in periods.iter().copied() {
            let target = fixed_window_boresight(
                &baseline,
                period,
                earliest,
                guarded_half_angle_rad,
                fov_half_angle_rad,
            );

            let start_floor = previous_period
                .map(|previous: UnsafePeriod| previous.last)
                .unwrap_or(earliest);
            let direct = add_fixed_target(
                &baseline,
                earliest,
                start_floor,
                period,
                &commands,
                target,
                self.config.max_slew_rate_rad_s,
            );

            commands = if let Some(previous) = previous_period {
                let return_index = previous.last + 1;
                if return_index < period.first {
                    let mut via_nadir = commands.clone();
                    via_nadir.push(ModelCommand {
                        sample_index: return_index,
                        target: ModeledTarget::Nadir,
                    });
                    via_nadir = canonicalize_model_commands(via_nadir);
                    let via_nadir = add_fixed_target(
                        &baseline,
                        earliest,
                        return_index,
                        period,
                        &via_nadir,
                        target,
                        self.config.max_slew_rate_rad_s,
                    );
                    choose_route(
                        &baseline,
                        earliest,
                        period.last + 1,
                        direct,
                        via_nadir,
                        self.config.max_slew_rate_rad_s,
                        fov_half_angle_rad,
                    )
                } else {
                    direct
                }
            } else {
                direct
            };
            previous_period = Some(period);
        }

        if let Some(last_period) = previous_period {
            let return_index = last_period.last + 1;
            if return_index < baseline.len() {
                let mut return_to_nadir = commands.clone();
                return_to_nadir.push(ModelCommand {
                    sample_index: return_index,
                    target: ModeledTarget::Nadir,
                });
                commands = choose_route(
                    &baseline,
                    earliest,
                    baseline.len() - 1,
                    commands,
                    canonicalize_model_commands(return_to_nadir),
                    self.config.max_slew_rate_rad_s,
                    fov_half_angle_rad,
                );
            }
        }

        let modeled_boresights = modeled_boresights(
            &baseline,
            earliest,
            &commands,
            self.config.max_slew_rate_rad_s,
        );
        let modeled_score = score_boresights(
            &baseline,
            &modeled_boresights,
            earliest,
            baseline.len() - 1,
            fov_half_angle_rad,
        );
        let commands = lift_commands(&baseline, &commands);
        let selected_schedule = self.selected_pointing_schedule(
            &accepted_schedule,
            baseline[earliest].time_mjd,
            &commands,
        );
        let validated_samples = self
            .run_geometry_simulation(telemetry, &selected_schedule)
            .await
            .context("failed to validate selected pointing schedule")?;
        let validation = self.validation_report(
            &validated_samples,
            baseline[earliest].time_mjd,
            fov_half_angle_rad,
        );

        Ok(PlanningOutcome::Schedule(CoorbitalEvasionPlan {
            earliest_command_mjd: baseline[earliest].time_mjd,
            horizon_end_mjd,
            commands,
            modeled_score,
            validation,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ThreatGeometry;

    fn sample(time_secs: f64, nadir: Vector3<f64>, threats: Vec<ThreatGeometry>) -> GeometrySample {
        GeometrySample {
            time_mjd: 60_000.0 + time_secs / SECONDS_PER_DAY,
            position_eci: -nadir,
            velocity_eci: Vector3::y(),
            attitude_body_to_eci: UnitQuaternion::identity(),
            boresight_eci: nadir,
            threats,
        }
    }

    fn threat(spacecraft_minus_threat: Vector3<f64>, visible: bool) -> ThreatGeometry {
        ThreatGeometry {
            relative_position_eci: spacecraft_minus_threat,
            line_of_sight: visible,
            in_field_of_view: false,
        }
    }

    #[test]
    fn eds_relative_position_is_negated_for_spacecraft_to_threat_los() {
        assert_relative_eq(
            threat_los_direction(&Vector3::new(-2.0, 0.0, 0.0)),
            Vector3::x(),
        );
    }

    fn assert_relative_eq(actual: Vector3<f64>, expected: Vector3<f64>) {
        assert!(
            (actual - expected).norm() < 1.0e-12,
            "{actual:?} != {expected:?}"
        );
    }

    #[test]
    fn unsafe_periods_require_visibility_and_preserve_safe_gaps() {
        let inside = threat(Vector3::new(0.0, 0.0, 1.0), true);
        let occulted = threat(Vector3::new(0.0, 0.0, 1.0), false);
        let samples = vec![
            sample(0.0, -Vector3::z(), vec![inside.clone()]),
            sample(1.0, -Vector3::z(), vec![inside]),
            sample(2.0, -Vector3::z(), vec![occulted]),
            sample(3.0, -Vector3::z(), vec![threat(Vector3::z(), true)]),
        ];
        assert_eq!(
            unsafe_periods(&samples, 31_f64.to_radians()),
            vec![
                UnsafePeriod { first: 0, last: 1 },
                UnsafePeriod { first: 3, last: 3 }
            ]
        );
    }

    #[test]
    fn fixed_window_target_is_safe_for_every_sample() {
        let samples = vec![
            sample(0.0, Vector3::z(), vec![threat(-Vector3::z(), true)]),
            sample(1.0, Vector3::z(), vec![threat(-Vector3::z(), true)]),
            sample(2.0, Vector3::z(), vec![threat(-Vector3::z(), true)]),
        ];
        let target = fixed_window_boresight(
            &samples,
            UnsafePeriod { first: 0, last: 2 },
            0,
            31_f64.to_radians(),
            30_f64.to_radians(),
        );
        assert!(fixed_direction_is_safe(
            &samples,
            0,
            2,
            target,
            31_f64.to_radians()
        ));
        assert!((target.dot(&Vector3::z()) - 31_f64.to_radians().cos()).abs() < 1.0e-10);
    }

    #[test]
    fn slew_step_respects_angular_rate_limit() {
        let current = Vector3::x();
        let target = Vector3::y();
        let next = slew_step(current, target, 0.1);
        assert!((angle_between(&current, &next) - 0.1).abs() < 1.0e-12);
    }

    #[test]
    fn simultaneous_threats_count_once_in_union_exposure() {
        let threats = vec![threat(-Vector3::z(), true), threat(-Vector3::z(), true)];
        let samples = vec![
            sample(0.0, Vector3::z(), threats),
            sample(10.0, Vector3::z(), vec![]),
        ];
        let score = score_boresights(
            &samples,
            &[Vector3::z(), Vector3::z()],
            0,
            1,
            30_f64.to_radians(),
        );
        assert!((score.exposure_secs - 10.0).abs() < 1.0e-6);
    }

    #[test]
    fn accepted_schedule_without_actual_visible_fov_entry_is_unchanged() {
        let mut occulted_in_fov = threat(-Vector3::z(), false);
        occulted_in_fov.in_field_of_view = true;
        let visible_outside_fov = threat(-Vector3::z(), true);
        let samples = vec![sample(
            0.0,
            Vector3::z(),
            vec![occulted_in_fov, visible_outside_fov],
        )];
        assert!(!accepted_schedule_exposed(&samples));
    }

    #[test]
    fn fixed_window_target_checks_all_visible_threats() {
        let samples = vec![
            sample(
                0.0,
                Vector3::z(),
                vec![threat(-Vector3::z(), true), threat(-Vector3::x(), true)],
            ),
            sample(
                1.0,
                Vector3::z(),
                vec![threat(-Vector3::z(), true), threat(-Vector3::x(), true)],
            ),
        ];
        let target = fixed_window_boresight(
            &samples,
            UnsafePeriod { first: 0, last: 1 },
            0,
            31_f64.to_radians(),
            30_f64.to_radians(),
        );
        assert!(fixed_direction_is_safe(
            &samples,
            0,
            1,
            target,
            31_f64.to_radians()
        ));
    }

    #[test]
    fn lifting_maps_body_z_and_canonicalizes_sign() {
        let reference = UnitQuaternion::identity();
        let lifted = lift_boresight(&reference, Vector3::x());
        assert_relative_eq(lifted * Vector3::z(), Vector3::x());
        assert!(
            lifted
                .quaternion()
                .coords
                .dot(&reference.quaternion().coords)
                >= 0.0
        );
    }

    #[test]
    fn latest_start_that_can_avoid_exposure_wins() {
        let samples = (0..=4)
            .map(|time| sample(time as f64, Vector3::z(), vec![]))
            .collect::<Vec<_>>();
        let target = Vector3::new(0.0, 1.0_f64.sin(), 1.0_f64.cos());
        let start = latest_reachable_start(
            &samples,
            0,
            0,
            3,
            &[ModelCommand {
                sample_index: 0,
                target: ModeledTarget::Nadir,
            }],
            target,
            1.0,
        );
        assert_eq!(start, 2);
    }

    #[test]
    fn unreachable_target_starts_at_earliest_allowed_sample() {
        let samples = (0..=4)
            .map(|time| sample(time as f64, Vector3::z(), vec![]))
            .collect::<Vec<_>>();
        let start = latest_reachable_start(
            &samples,
            0,
            0,
            3,
            &[ModelCommand {
                sample_index: 0,
                target: ModeledTarget::Nadir,
            }],
            Vector3::x(),
            0.1,
        );
        assert_eq!(start, 0);
    }

    #[test]
    fn command_at_sample_does_not_change_previous_interval() {
        let samples = (0..=2)
            .map(|time| sample(time as f64, Vector3::x(), vec![]))
            .collect::<Vec<_>>();
        let commands = vec![
            ModelCommand {
                sample_index: 0,
                target: ModeledTarget::Nadir,
            },
            ModelCommand {
                sample_index: 1,
                target: ModeledTarget::Fixed(Vector3::y()),
            },
        ];

        let boresights = modeled_boresights(&samples, 0, &commands, 10.0);

        assert_relative_eq(boresights[1], Vector3::x());
        assert_relative_eq(boresights[2], Vector3::y());
    }

    #[test]
    fn later_period_can_use_a_distinct_fixed_target() {
        let mut samples = (0..=8)
            .map(|time| sample(time as f64, Vector3::z(), vec![]))
            .collect::<Vec<_>>();
        samples[2].threats = vec![threat(-Vector3::z(), true)];
        samples[6].threats = vec![threat(-Vector3::z(), true)];
        let periods = unsafe_periods(&samples, 31_f64.to_radians());
        let mut commands = vec![ModelCommand {
            sample_index: 0,
            target: ModeledTarget::Nadir,
        }];

        for (index, period) in periods.iter().copied().enumerate() {
            let target = fixed_window_boresight(
                &samples,
                period,
                0,
                31_f64.to_radians(),
                30_f64.to_radians(),
            );
            let direct = add_fixed_target(
                &samples,
                0,
                periods
                    .get(index.wrapping_sub(1))
                    .map(|previous| previous.last)
                    .unwrap_or(0),
                period,
                &commands,
                target,
                10.0,
            );
            commands = direct;
        }

        let fixed_commands = commands
            .iter()
            .filter(|command| matches!(command.target, ModeledTarget::Fixed(_)))
            .collect::<Vec<_>>();
        assert_eq!(fixed_commands.len(), 2);
        assert!(fixed_commands[0].sample_index <= periods[0].first);
        assert!(fixed_commands[1].sample_index > periods[0].last);
    }
}
