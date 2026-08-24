# Co-Orbital Threat Evasion Autonomy Mode

This document describes the co-orbital threat evasion autonomy mode. The mode commands spacecraft attitude to minimize exposure to known threats while maintaining alignment with Nadir. It uses a satellite EDS for simulation, planning, and validation.

## Current Implementation

The implementation identifies sampled periods where Nadir would expose configured threats. For each period it solves for one fixed body-to-ECI boresight that satisfies every visible threat/sample constraint, schedules the latest reachable slew, and compares direct transitions with transitions through Nadir.

The selected schedule is scored with the sampled constant-rate model and validated once through finite-attitude-dynamics EDS before being reconciled with the command board. The planner is deterministic and heuristic; it does not claim global continuous-time optimality.

Scheduled `PointQuaternion` commands use vector-first `xyzw` body-to-ECI quaternions. The FOV constraint applies to body `+Z`; quaternion roll is selected after the target boresight is chosen.

## Mission Model And Requirements

The spacecraft has a circular field of view (FOV) fixed in its body frame. In the spacecraft model:

- The FOV boresight is body `+Z`.
- The FOV half-angle is `alpha`.
- The desired mission direction is Nadir, defined as the negative spacecraft position vector.
- A configured threat constrains pointing only when its spacecraft line of sight is geometrically clear; threats occulted by Earth do not constrain pointing. The planner must account for threats that become visible within the planning horizon.
- Ground and space threat trajectories are perfectly known over the planning horizon by running the EDS with the appropriate threat configuration. The planner does not account for uncertainty in threat position or emission state; EDS results are deterministic and authoritative.

The planner shall minimize the time that threats are in the FOV. Among plans with equal minimum exposure, it shall keep the boresight as closely aligned with Nadir as possible.

- A commanded pointing is not reached instantaneously. The planner must avoid exposure during slews to target pointings.
- Zero exposure is preferred but is not guaranteed to be feasible. If every available trajectory has exposure, the mode shall emit the least-exposure trajectory and log a warning; it shall not fail to emit a plan.
- When no threat is expected to enter the FOV, the planner does not need to alter existing accepted pointing commands. It only needs to propose new pointing commands when threats will enter the FOV.

## Planner Workflow

The planner is a **sampled-series, receding-horizon fixed-window planner**:

1. Run the EDS to obtain sampled spacecraft and threat trajectories over a finite horizon.
2. Identify contiguous periods where Nadir is inside the guarded exclusion region.
3. For each period, solve for the fixed boresight closest to the duration-weighted Nadir direction while satisfying every visible threat/sample constraint.
4. Lift each selected boresight into a minimum-change body-to-ECI quaternion.
5. Schedule the latest constant-rate slew that can reach each target.
6. Compare direct transitions with transitions that return through Nadir.
7. Score the complete modeled schedule.
8. Run the selected command schedule through the finite-attitude-dynamics EDS once.
9. Propose the selected scheduled pointing commands to the SAFE command board.
10. Repeat from new telemetry while the mode is active.

The EDS produces time-series fields only; it does not produce event records or perform post-processing. SAFE may derive approximate visibility and FOV crossing times from those series when useful. The initial implementation may instead operate directly on every sampled time without deriving crossings.

Receding-horizon planning is necessary because telemetry, current attitude, wheel state, approved commands, and the usable forecast horizon change over time. Event-driven EDS output is not available.

## EDS Output Reference

The following EDS time-series fields are relevant to the planner.

### Spacecraft

- `in_shadow` (boolean): whether the spacecraft is in Earth's shadow
- `position` (ECI)
- `velocity` (ECI)
- `latitude_rad`
- `longitude_rad`
- `altitude_km`
- `semi_major_axis_km`
- `eccentricity`
- `inclination_rad`
- `raan_rad`
- `argument_of_periapsis_rad`
- `true_anomaly_rad`
- `eccentric_anomaly_rad`
- `mean_anomaly_rad`
- `commanded_attitude`: quaternion from body to ECI
- `pointing_error` (degrees): angle between commanded and actual boresight
- `attitude`
- `angular_velocity` (radians/s): 3-vector of angular velocity in the body frame
- `ecef_attitude`
- `body_x_nadir_angle` (radians): angle between body `+X` and Nadir
- `body_y_nadir_angle` (radians): angle between body `+Y` and Nadir
- `body_z_nadir_angle` (radians): angle between body `+Z` and Nadir

### Attitude Control System

For each reaction wheel:

- `commanded_torque` (N*m)

For each magnetorquer:

- `commanded_moment` (A*m^2)
- `torque`

### Field Of View

- `boresight_eci` (ECI): unit vector of the FOV boresight
- `commanded_boresight_eci` (ECI): unit vector of the commanded FOV boresight

### Threats

For each threat:

- `position` (ECI)
- `velocity` (ECI)
- `relative_position_eci` (3-vector, km, ECI): threat-to-spacecraft vector
- `relative_velocity_eci` (3-vector, km/s, ECI): threat-to-spacecraft velocity
- `range` (km): distance to threat
- `range_rate` (km/s): radial velocity to threat
- `line_of_sight` (boolean): whether the threat is geometrically visible from the spacecraft
- `off_boresight_angle` (radians): angle between FOV boresight and threat relative position
- `fov_clearance` (radians): `off_boresight_angle - alpha`
- `in_field_of_view` (boolean): whether the threat is in the FOV

## Notation

All vectors in the following equations are unit vectors unless stated otherwise.

- `r_s(t)`: spacecraft inertial position.
- `v_s(t)`: spacecraft inertial velocity.
- `r_i(t)`: position of threat `i`.
- `u_i(t)`: spacecraft-to-threat line-of-sight direction.
- `n(t)`: Nadir direction.
- `b(t)`: actual FOV boresight direction.
- `b_target(t)`: commanded target boresight direction.
- `alpha`: FOV half-angle.
- `alpha_plan`: half-angle used by the planner, including a numerical clearance guard.
- `theta_i(t)`: angle between the actual boresight and threat `i`.
- `theta_nadir(t)`: angle between the actual boresight and Nadir.
- `c_i(t)`: signed angular clearance from threat `i`.
- `J`: objective or cost used to compare plans. This is conventional optimization notation, not a physical simulation field.

```text
n(t)            = -r_s(t) / ||r_s(t)||
u_i(t)          = (r_i(t) - r_s(t)) / ||r_i(t) - r_s(t)||
theta_i(t)      = atan2(||b(t) x u_i(t)||, b(t) . u_i(t))
theta_nadir(t)  = atan2(||b(t) x n(t)||, b(t) . n(t))
c_i(t)          = theta_i(t) - alpha
```

Positive `c_i` is outside the physical FOV, zero is on its boundary, and negative `c_i` is inside the FOV. Clamp dot products to `[-1, 1]` when an inverse trigonometric function is unavoidable. Feasibility tests should normally use dot products directly.

## Autonomy Mode Configurable Parameters

- `eds_path`: path to the EDS executable
- `gnc_time_step_limits`: minimum and maximum time step for the GNC engine in seconds
- `cdh_time_step_limits`: minimum and maximum time step for the CDH engine in seconds
- `power_time_step_limits`: minimum and maximum time step for the Power engine in seconds
- `simulation_timeout_secs`: maximum time to wait for the EDS to complete in seconds
- `min_replan_interval_secs`: minimum time between replans in seconds
- `agent_id`: ID of the main spacecraft agent in the simulation
- `field_of_view_id`: ID of the FOV in the simulation
- `fov_half_angle`: half-angle of the FOV in degrees
- `fov_guard_angle`: additional angle guard for planning in degrees. When relevant, added to `fov_half_angle` to produce `alpha_plan`.
- `threat_ids`: list of threat IDs to consider in the planner and when running the EDS. The planner ignores threats not in this list.
- `ground_threat_locations`: dictionary of ground threat IDs to their latitude, longitude, and altitude locations. The locations are given in a list `[latitude (deg), longitude (deg), altitude (km)]`.
- `space_threat_epoch_states`: dictionary of space threat IDs to their state at a given epoch. The state is given in a list `[epoch (MJD), position (km, ECI), velocity (km/s, ECI)]`.
- `planning_horizon`: time horizon for planning in days
- `command_lead_secs`: time in seconds from the current time to the permitted start of the commanded pointing. The planner may propose commands that start at or after this time. This is meant to account for command processing and verification delays.

## Exclusion-Cap Geometry

At time `t`, each visible threat excludes boresights inside a spherical cap centered on `u_i(t)`.
Use

```text
alpha_plan = alpha + fov_guard_angle
```

where the guard accounts only for a safety margin for numerical error or time discretization.
No threat-state uncertainty margin is required by the current assumptions.

A boresight `b` is planning-feasible for threat `i` when

```text
dot(u_i, b) <= cos(alpha_plan).
```

The instantaneous Nadir-alignment problem is

```text
maximize    dot(n, b)
subject to  dot(u_i, b) <= cos(alpha_plan) for every visible threat i
            dot(b, b) = 1.
```

Maximizing `dot(n, b)` is equivalent to minimizing the angle between `n` and `b`.

### Fixed-window target

For a complete unsafe window, a fixed target `b` is feasible when it satisfies every visible threat
constraint at every EDS sample in the window:

```text
dot(u_i[k], b) <= cos(alpha_plan)
```

The stationary-window Nadir objective is equivalent to maximizing `dot(b, N)`, where

```text
N = sum_k(dt[k] * n[k]).
```

The implementation starts with `normalize(N)` and repeatedly projects it to the boundary of the
most violated threat cap. Each projected direction is checked against every threat/sample
constraint. The projection is bounded and deterministic. If no feasible direction is found, the
best sampled-exposure direction considered by the projection is emitted as a best-effort target.

The target is a boresight direction rather than a quaternion. Quaternion roll is selected after
the target is chosen, using the minimum-change body-to-ECI lifting rule.
