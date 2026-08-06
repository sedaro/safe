# `safectl` Reference

`safectl` communicates with a running SAFE process through the local Unix
socket at `<base_writable_directory>/state/safectl.sock`. It resolves the
runtime YAML path using the same rules as SAFE and reads the writable-directory
setting from that YAML file.

Run it from the repository root during local development:

```bash
cargo run -p safectl -- <command>
```

The installed binary can be invoked as `safectl <command>`.

## Output Formats

Commands that render records support `--output table` and `--output json`. Table
is the default for snapshots. `logs` uses readable line output by default and
emits one JSON object per line with `--output json`, which also works with
`--follow`.

Human-readable tables use UTF-8 borders, right-align numeric columns, wrap long
values, and escape control characters so names, reasons, and payload previews
cannot shift the surrounding columns. Resource values use binary units (`B`,
`KiB`, `MiB`, `GiB`, and `TiB`). Mode state labels and timed commands are
expanded into operator-friendly text instead of Rust debug representations.

Snapshot watches such as `status --watch`, `watch board`, and `top modes
--watch` redraw in a terminal. When output is redirected, each changed snapshot
is separated by a timestamp. `watch telemetry` renders its table header and
redraws the current tail as single-line formatted rows; payload previews
are truncated to fit the available terminal width. Redirected watch output
appends timestamped snapshots instead of using terminal control sequences.

## Status and Modes

```bash
safectl status
safectl status --watch --interval-secs 2
safectl status --output json
safectl get modes
safectl get modes --all
safectl get modes ExampleMode --output json
safectl describe mode ExampleMode
safectl describe mode ExampleMode --output json
```

`status` reports daemon state, latest telemetry, board counts, and mode
connection/handler state. `get modes` uses the operational status snapshot when
available and falls back to the mode configuration and flight checkpoint.
Disabled modes are hidden unless `--all` is supplied. `describe mode` includes
the activation JSON and mode-specific configuration.

Mode IDs are deterministic UUID v5 values derived from mode names. Use the mode
name with the CLI; use the UUID in protocol and state-file integrations.

## Telemetry

```bash
safectl get telemetry
safectl get telemetry --tail 20 --source example
safectl get telemetry --output json
safectl watch telemetry --tail 10
safectl watch telemetry --source example --output json
```

`get telemetry` reads `state/events.jsonl` and falls back to the latest frame in
`state/status.json` when no matching event is available. `watch telemetry`
prints a tail and follows new events until interrupted.

Payload-only JSON is accepted for quick local tests:

```bash
safectl send telemetry --json '{"telemetry":{"temperature_value_c":34.5}}'
```

This produces a frame with no source and `ts_mono: 0`. To provide a source and
timestamp, a plain telemetry frame with an object payload is accepted:

```bash
safectl send telemetry --json '{"source":"sensor","ts_mono":42,"payload":{"environment":{"temperature_c":20.0}}}'
```

The full ingress shape remains supported. `TelemetryFrame.payload` is encoded
as a JSON string by that serde contract:

```bash
safectl send telemetry --json '{"type":"telemetry","telemetry":{"source":"example","ts_mono":42,"payload":"{\"telemetry\":{\"temperature_c\":20.0}}"}}'
```

The external telemetry adapter accepts decoded payload objects directly; see
[`runtime-config.md`](./runtime-config.md).

## Command Ingress

The general form is:

```bash
safectl send command --json '<JSON>'
```

When neither `--json` nor a helper operation is supplied, `safectl` opens
`$EDITOR` (default `vi`) with a template. A command request receives an
automatically generated request ID and prints it after sending.

For common operations, use helpers:

```bash
safectl send --op activate_mode --mode ExampleMode
safectl send --op deactivate_mode --mode ExampleMode
safectl send --op restart_mode --mode ExampleMode
safectl send --op stop_mode --mode ExampleMode
safectl send --op execute_now --command PointNadir
```

Supported `execute_now` names are `SetPidControllerGains`, `IridiumPowerOn`,
`IridiumPowerOff`, `PointSunYaw`, `PointNadir`, `CaptureImage`, `PointThruster`,
`ThrusterOn`, and `ThrusterOff`. The helper uses zero gains for
`SetPidControllerGains`; use JSON when command arguments matter.

The full ingress enum has these forms:

```json
{
  "type": "command",
  "command": {
    "kind": "execute_now",
    "command": "PointNadir"
  },
  "request_id": "operator-request-1"
}
```

```json
{
  "type": "command",
  "command": {
    "kind": "activate_mode",
    "mode": "00000000-0000-0000-0000-000000000000"
  }
}
```

For mode operations, the helper is safer because it derives the UUID from the
mode name. The socket reader accepts one JSON message per line and logs invalid
lines without returning a structured error to the client.

## Board and Requests

```bash
safectl get board
safectl get board --state pending
safectl get board --state approved --output json
safectl watch board --state pending
safectl get request safectl:<request-uuid>
```

Board states are `pending`, `approved`, `rejected`, and `published`. A request
ID identifies host-command status records in
`state/host_command_status.jsonl`; it does not guarantee that a host vehicle
executed the command.

## Logs

```bash
safectl logs
safectl logs --mode-name ExampleMode --follow
safectl logs ExampleMode --tail 200
safectl logs --all-modes --follow
safectl logs --level warn --filter heartbeat
safectl logs --since 2026-08-06T12:00:00Z --before 2026-08-06T13:00:00Z
safectl logs --all-modes --output json
```

The positional mode name and `--mode-name` are equivalent. `--id` accepts a
mode UUID. `--all-modes` merges mode records by timestamp and includes the mode
name in the output. Timestamp filters require RFC3339 values and exclude records
without a parseable timestamp. Logs are found in the parent directory of
`logging.file_path`, with `default.log` for unscoped events and `<mode-uuid>.log`
for mode events. Older `AutonomyModeId_<uuid>_.log` files are read and merged
with the canonical file during migration.

Each structured log record contains `timestamp`, `level`, `target`, `mode_id`,
`stream`, `message`, and `fields`. `stream` is `safe`, `stdout`, or `stderr`.
Human output is colorized only when stdout is a terminal and escapes control
characters before printing.

## Event and Effect Streams

```bash
safectl watch messages
safectl watch messages --kind events --tail 50
safectl watch messages --kind effects --follow
```

`events` reads `state/events.jsonl`; `effects` reads `state/outputs.jsonl`.
These are append-only JSONL streams used for inspection and recovery. Tick
events are not written to the event log to keep it compact.

## Resource Usage

```bash
safectl top modes
safectl top modes --all --output json
safectl top modes --watch
safectl top modes --tui
```

The TUI refreshes at the configured interval. Press `c` to toggle child
processes, `q` or Escape to quit, and `Ctrl-C` to quit. Resource snapshots are
written by the sandbox metrics task every five seconds. The current sandbox
writer and CLI use different default directory layouts; treat missing metrics
as a known integration limitation until those paths are reconciled.

## Help

Use Clap's generated help for the exact installed version:

```bash
cargo run -p safectl -- --help
cargo run -p safectl -- send --help
cargo run -p safectl -- logs --help
```
