# Activation Configuration

SAFE selects one desired autonomy mode from the entries in
`autonomy_mode_config.json`. The file is a top-level JSON array. The checked-in
file is currently empty, so it is a valid configuration with no modes.

## Selection Order

For each telemetry frame, SAFE applies this order:

1. An enabled mode selected by a manual override remains pinned.
2. Without a manual override, an active hysteretic mode is retained while its
   `exit` expression evaluates to `false`.
3. Otherwise, SAFE filters to enabled modes whose activation condition is true
   and selects the highest-priority eligible mode.

An activation evaluation error makes a mode ineligible during normal selection.
If evaluation of the current hysteretic mode's `exit` expression fails, SAFE
keeps that mode active rather than switching away from it.

Manual overrides are issued through `safectl send --op activate_mode`. The
override is cleared by `deactivate_mode`, by disabling the mode, or when the
mode is removed from configuration.

## Mode Entry Schema

Each array entry has this shape:

| Field | Required | Description |
| --- | --- | --- |
| `name` | yes | Unique mode name. It deterministically derives the mode UUID. |
| `priority` | yes | Unsigned priority used for selection. |
| `enabled` | no | Defaults to `true`. Disabled entries remain in metadata but are not spawned. |
| `bin_path` | yes | Mode executable. Relative paths are resolved relative to this JSON file. |
| `args` | no | Additional executable arguments. Defaults to `[]`. |
| `sandbox_resources` | yes | `cpu`, `memory`, and `disk` limits. |
| `persist_work_dir` | no | Preserve the mode working directory across restarts. Defaults to `false`. |
| `mode_config` | no | Arbitrary JSON passed to the mode. Defaults to `null`. |
| `activation` | no | `Immediate` or `Hysteretic` activation rule. |

Mode IDs are UUID v5 values derived from `name` using the UUID OID namespace.
Changing a name creates a new mode identity. `limits.max_autonomy_modes` counts
all entries, including disabled entries.

Example entry:

```json
{
  "name": "ExampleMode",
  "priority": 10,
  "enabled": true,
  "bin_path": "../target/debug/mode_llm_advisor",
  "args": [],
  "sandbox_resources": {
    "cpu": 25.0,
    "memory": 536870912,
    "disk": 104857600
  },
  "persist_work_dir": true,
  "mode_config": {},
  "activation": {
    "Immediate": {
      "GreaterThan": [
        {
          "Term": {
            "Float64": {
              "TelemetryRef": "telemetry.temperature_value_c"
            }
          }
        },
        {
          "Term": {
            "Float64": {
              "Literal": 34.0
            }
          }
        }
      ]
    }
  }
}
```

The example is a schema example only. `mode_llm_advisor` requires its own
validated `mode_config`; see its README for a complete advisor configuration.

## Activation Types

Omit `activation` for a mode that is always eligible. Use the externally tagged
JSON enum forms below:

```json
{ "Immediate": { "Term": { "Bool": { "Literal": true } } } }
```

Use `Timed` to activate a mode from a condition for a bounded window:

```json
{
  "Timed": {
    "condition": { "Term": { "Bool": { "TelemetryRef": "fault.active" } } },
    "duration_secs": 30
  }
}
```

The timer starts when the mode is selected, and expires independently of
telemetry arrival. A true condition is latched after expiry; it must become
false before a later true condition can start a new window. Manual overrides
continue to pin a mode until explicitly deactivated.

```json
{
  "Hysteretic": {
    "enter": {
      "GreaterThan": [
        { "Term": { "Float64": { "TelemetryRef": "thermal.value_c" } } },
        { "Term": { "Float64": { "Literal": 25.0 } } }
      ]
    },
    "exit": {
      "LessThan": [
        { "Term": { "Float64": { "TelemetryRef": "thermal.value_c" } } },
        { "Term": { "Float64": { "Literal": 23.0 } } }
      ]
    }
  }
}
```

`Expr` supports `Term`, `And`, `Or`, `Not`, `GreaterThan`, `LessThan`, and
`Equal`. `Variable` supports `Bool`, `Float64`, and `String`.

`Value<T>` supports these forms:

| Form | Current behavior |
| --- | --- |
| `Literal` | Supported. |
| `TelemetryRef` | Supported for scalar values in the latest telemetry payload. |
| `AverageTelemetryRef` | Numeric and boolean averages use recent history. String averages are not implemented. |
| `VariableRef` | Reserved but currently unresolved because no runtime variables are provided. |
| `LastPlannedAutonomyModeRef` | String resolution is available; numeric and boolean forms are not implemented. |

Telemetry paths are dot-separated paths into `TelemetryFrame.payload`. Numeric
segments index JSON arrays. The frame `source` and `ts_mono` fields are not part
of a telemetry path. Numeric and boolean average lookups use at most the latest
256 telemetry frames.

## Reload Behavior

SAFE polls the mode configuration approximately once per second and reloads it
when the file contents change. Added and removed modes are reconciled. Changes
to `bin_path`, `args`, `sandbox_resources`, `persist_work_dir`, or `mode_config`
restart the affected mode process. Priority, enabled state, and activation
changes update selection metadata without requiring a process restart.

An invalid reload is logged and the last valid configuration remains active.
The initial configuration is required to parse successfully before SAFE starts.

## Manual Control

For a configured mode named `ExampleMode`:

```bash
cargo run -p safectl -- send --op activate_mode --mode ExampleMode
cargo run -p safectl -- get modes --all
cargo run -p safectl -- send --op deactivate_mode --mode ExampleMode
```

Use `--mode-name` or a positional mode name with `safectl logs`; `--mode` is a
helper option for `send`, not for `logs`:

```bash
cargo run -p safectl -- logs --mode-name ExampleMode --follow
cargo run -p safectl -- describe mode ExampleMode
```

There is no checked-in `NoImages` or `NoImagesHot` executable in this workspace.
Do not use older recipes that expect those binaries or modes to exist.
