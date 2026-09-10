# Mission Planning Autonomy Mode

`mode-mission-planning` creates a deterministic, simulation-backed schedule for
generic observation and ground-contact operations. It contains no spacecraft,
agent, component, target, or ground-station identifiers. Deployment-specific
state projection is delegated to the same external input-adapter contract used
by `safe-gatekeeper`.

## Behavior

The mode plans once per activation. It uses the latest telemetry received while
inactive, or waits for the first telemetry frame after activation. A second
activation within `min_replan_interval_secs` emits one explicit `NOOP` instead
of replacing the previous plan.

Planning performs two simulations:

1. A baseline run includes only commands in the board source of truth. Its
   configured output fields provide time, state of charge, target-in-FoV flags,
   and ground-station elevations.
2. A candidate run includes the source-of-truth commands plus the proposed
   schedule. EDS must succeed and every `validation_checks` entry must pass
   before any candidate command is emitted.

Priority is deterministic:

1. State of charge at or below `low_power_state_of_charge` selects
   `PointSunYaw` until a sample reaches `recovered_state_of_charge`.
2. Otherwise, a station at or above `minimum_elevation_deg` selects `Track` for
   that station's configured latitude, longitude, and altitude. Configuration
   order breaks simultaneous-station ties.
3. Otherwise, the configured `default_pointing` is selected.

Captures are suppressed during low-power and contact intervals. Target FoV
flags are merged, and each contiguous eligible interval produces one
`CaptureImage` at its sampled midpoint. Existing approved equivalent board
commands are not emitted again.

The replan timestamp and power-hysteresis latch are process-local. A future
improvement could persist them across supervisor or host restarts.

## Adapter Contract

For both runs, the configured adapter receives `SimulationInputRequest` JSON on
stdin and returns `SimulationInputResponse` JSON on stdout. This is the contract
documented in [`safe-gatekeeper`](../safe-gatekeeper/README.md#adapter-contract).
The adapter owns mission-specific telemetry decoding, EDS identifiers, units,
and conversion of the complete command schedule into EDS patches.

## Configuration

This illustrative mode configuration intentionally uses placeholders:

```json
{
  "eds_path": "/path/to/eds/workspace",
  "input_adapter_command": ["/path/to/input-adapter", "mission-planning-input"],
  "input_adapter_config": {},
  "input_adapter_timeout_secs": 30,
  "simulation_timeout_secs": 120,
  "planning_horizon_secs": 21600.0,
  "min_replan_interval_secs": 300,
  "command_lead_secs": 5.0,
  "command_dedup_tolerance_secs": 1.0,
  "telemetry_gps_time_pointer": "/telemetry/gps_time",
  "telemetry_state_of_charge_pointer": "/telemetry/state_of_charge",
  "result_file": "result.jsonl",
  "time_field": "time",
  "state_of_charge_field": "state_of_charge",
  "low_power_state_of_charge": 0.3,
  "recovered_state_of_charge": 0.6,
  "minimum_elevation_deg": 10.0,
  "default_pointing": { "kind": "nadir" },
  "targets": [
    { "name": "example-target", "in_fov_field": "target.in_fov" }
  ],
  "ground_stations": [
    {
      "name": "example-station",
      "latitude_deg": 0.0,
      "longitude_deg": 0.0,
      "altitude_m": 0.0,
      "elevation_field": "station.elevation_deg"
    }
  ],
  "validation_checks": [
    {
      "target_file": "result.jsonl",
      "field": "constraint_margin",
      "aggregation": "min",
      "op": "gte",
      "threshold": 0.0
    }
  ]
}
```

`default_pointing.kind` supports `nadir`, `sun_yaw`, `quaternion`, and `ypr`.
Quaternion values use `x`, `y`, `z`, and `w`; YPR values use `roll_deg`,
`pitch_deg`, and `yaw_deg`.

The result file must contain every configured field in each planning sample.
FoV fields are booleans. Time, state of charge, station elevation, and
validation fields are floating-point values. Transition times use simulation
sample timestamps rather than interpolation.

## Verification

```bash
cargo test -p mode-mission-planning
```
