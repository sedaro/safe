# Activation Config Guide

SAFE can select the active autonomy mode from telemetry-driven activation rules.

At runtime, mode selection works like this:

1. If a manual override is set (for example, via `safectl send --op activate_mode`), that mode stays pinned while it remains enabled.
2. Otherwise, if the current active mode has hysteresis and its `exit` is `false`, SAFE keeps that mode active.
3. Otherwise, SAFE evaluates mode activation eligibility and picks the highest-priority eligible enabled mode.

If an activation expression fails to evaluate, that mode is treated as ineligible for that cycle.

## `activation` field

In `autonomy_mode_config.json`, each mode may include an optional `activation` field.

- Omitted `activation`: mode is always eligible (current priority-only behavior).
- `Immediate(expr)`: mode is eligible when `expr` evaluates to `true`.
- `Hysteretic { enter, exit }`:
  - mode is eligible to become active when `enter` is `true`
  - once active, SAFE keeps it active until `exit` becomes `true`

Because this is Rust enum JSON, use externally-tagged enum shape.

## Expression JSON shape

`Expr` supports:

- `Term(Variable)`
- `And([Expr, ...])`
- `Or([Expr, ...])`
- `Not(Expr)`
- `GreaterThan(Expr, Expr)`
- `LessThan(Expr, Expr)`
- `Equal(Expr, Expr)`

`Variable` supports:

- `Bool(Value<bool>)`
- `Float64(Value<f64>)`
- `String(Value<String>)`

`Value<T>` supports:

- `Literal(T)`
- `TelemetryRef("dot.path")`
- `AverageTelemetryRef { "name": "dot.path", "points": N }`

## Telemetry paths

Telemetry references resolve against `Telemetry` fields using dot paths.

Examples:

- `"telemetry.temperature_value_c"`

## Example: immediate rule

```json
{
  "name": "NoImagesHot",
  "priority": 4,
  "enabled": true,
  "bin_path": "/home/wdaughtridge.guest/safe/target/debug/mode_no_images_hot",
  "args": [],
  "sandbox_resources": {
    "cpu": 90.0,
    "memory": 1000000000,
    "disk": 1000000000
  },
  "persist_work_dir": true,
  "activation": {
    "Immediate": {
      "GreaterThan": [
        { "Term": { "Float64": { "TelemetryRef": "telemetry.temperature_value_c" } } },
        { "Term": { "Float64": { "Literal": 34.0 } } }
      ]
    }
  },
  "mode_config": {
    "label": "NoImagesHot"
  }
}
```

## Example: hysteretic rule

```json
{
  "activation": {
    "Hysteretic": {
      "enter": {
        "GreaterThan": [
          { "Term": { "Float64": { "TelemetryRef": "telemetry.temperature_value_c" } } },
          { "Term": { "Float64": { "Literal": 25.0 } } }
        ]
      },
      "exit": {
        "LessThan": [
          { "Term": { "Float64": { "TelemetryRef": "telemetry.temperature_value_c" } } },
          { "Term": { "Float64": { "Literal": 23.0 } } }
        ]
      }
    }
  }
}
```

This creates a deadband: mode enters above `25.0` and does not release until below `23.0`.

## Live Test Recipe

Use this recipe with the sample `autonomy_mode_config.json` that includes:

- `NoImages` (`priority: 1`, immediate true baseline/fallback)
- `NoImagesHot` (`priority: 4`, immediate: activate when `rtd1_c > 34.0`)

### 1) Start SAFE

```bash
cargo run -p safe
```

In another terminal, watch active mode:

```bash
cargo run -p safectl -- get modes
```

Expected at startup: `NoImages` is active as the baseline fallback mode.

### 2) Trigger manual override pin

```bash
cargo run -p safectl -- send --op activate_mode --mode NoImagesHot
```

Then check modes:

```bash
cargo run -p safectl -- get modes
```

Expected: `NoImagesHot` stays active (pinned), regardless of scheduler conditions.

### 3) Clear manual pin and return to scheduler

```bash
cargo run -p safectl -- send --op deactivate_mode --mode NoImagesHot
```

Then check modes again:

```bash
cargo run -p safectl -- get modes
```

Expected: scheduler decides active mode again (usually `NoImages` unless hysteretic enter condition is true).

### 4) Validate hysteresis with telemetry replay (recommended)

Run SAFE against telemetry where `telemetry.temperature_value_c` crosses thresholds:

- above `34.0` -> `NoImagesHot` becomes eligible and should activate
- at or below `34.0` -> `NoImagesHot` becomes ineligible and scheduler can switch away

With the 3-mode sample, thresholds should typically behave like this:

- `rtd1_c > 34.0` -> `NoImagesHot` should win (highest priority)
- lower ranges -> fallback to `NoImages`

Use `safectl get modes` periodically to confirm transitions.

### 5) Optional observability

- Watch JSONL stream:

```bash
cargo run -p safectl -- watch messages --kind all -f
```

- Filter logs by mode:

```bash
cargo run -p safectl -- logs --mode NoImagesHot -f
```

You should see activate/deactivate behavior align with manual commands and hysteretic thresholds.

To specifically observe switching logs from the new no-op-like mode:

```bash
cargo run -p safectl -- logs --mode NoImagesHot -f
```

You should see `mode activated` / `mode deactivated` and `active telemetry` messages as thresholds are crossed.

You can inspect each mode's effective activation JSON and mode payload with:

```bash
cargo run -p safectl -- describe mode NoImagesHot
```
