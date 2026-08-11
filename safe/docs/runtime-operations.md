# Runtime Operations

This page describes the files and operational behavior produced by a running
SAFE daemon. The root directory is `base_paths.base_writable_directory`, which
defaults to `/tmp/safe`.

## Runtime Layout

```text
<base>/
  state/
    flight.json
    events.jsonl
    outputs.jsonl
    status.json
    safe.pid
    safectl.sock
    host_command_status.jsonl
    modes/
      <mode-uuid>/
        ipc.sock
        mode-config.json
  out/
    summary.json
    commands.csv
```

Logs are written beside the configured `logging.file_path` parent, not to a
single file with exactly the configured filename. SAFE creates `default.log`
and one `<mode-uuid>.log` file per mode. Each line is a JSON object containing
`timestamp` (RFC3339 UTC), `level`, `target`, `mode_id`, `stream`, `message`,
and structured `fields`. Mode stdout and stderr are kept in the mode stream and
are marked by `stream`.

SAFE also reads the previous sanitized
`AutonomyModeId_<uuid>_.log` filename through `safectl` so upgrades do not hide
existing supervisor records. The current logging implementation does not
rotate files even though rotation fields are validated in YAML.

The sandbox metrics writer additionally uses
`SAFE_METRIC_BASE_PATH/<mode-uuid>/metrics-current.json`. Default builds also
write `metrics.bin`; the size-optimized EDS feature omits that unused protobuf
output. `safectl top modes` currently searches under
`<base>/state/modes/`; this path mismatch can make valid metrics invisible.

## Startup and Recovery

On startup SAFE:

1. Loads or creates `flight.json` and `out/summary.json`.
2. Loads the autonomy-mode configuration and starts enabled modes.
3. Rebuilds the command board from board effects in `outputs.jsonl`.
4. Replays unapplied events from `events.jsonl`.
5. Rehydrates the desired active mode and sends a board snapshot to modes.
6. Writes the initial `status.json`.

Events with a sequence number no greater than the persisted flight sequence are
ignored during recovery. Invalid JSONL lines are skipped during replay. Keep
`flight.json`, `events.jsonl`, and `outputs.jsonl` together when backing up or
restoring runtime state.

## Event, Board, and Output Flow

Telemetry and external control requests become events. Mode commands become
board proposals identified by a deterministic event-based `BoardCmdId`. Board
events record proposal, approval, and cancellation state. The current source of
truth consists of proposed commands that have approval and no rejection.

Pending board batches are sent to the configured gatekeeper. With
`gatekeeper_adapter: disabled`, SAFE automatically approves them with the
detail `gatekeeper disabled`. An external gatekeeper receives JSONL requests on
stdin and returns JSONL `approve` or `reject` messages on stdout.

The default platform egress writes:

- Host command status records to `state/host_command_status.jsonl`.
- Approved scheduled commands to `out/commands.csv` with `cmd` and `gps_time`
  columns.

`TimedCommand::Now` and `TimedCommand::NOOP` are currently omitted from
`commands.csv`. The direct `ExecuteNow` path is recorded in SAFE effects, but
the host command dispatch integration is incomplete. A CLI status of
`dispatched` means SAFE accepted the request and queued its internal event; it
does not prove host execution.

## Operational Status

`state/status.json` contains a snapshot with:

- Daemon PID, running/halted state, fault, and last applied sequence.
- Telemetry count, latest source, timestamp, and payload.
- Board command states and decision details.
- Mode name, UUID, priority, enabled/eligible/active state, selection reason,
  and connection/handler status.

Use:

```bash
cargo run -p safectl -- status
cargo run -p safectl -- get modes --all
cargo run -p safectl -- get board
```

## Mode Supervision

Enabled mode binaries run in per-mode working directories under `state/modes`.
On Linux, sandbox isolation is selected automatically when `unshare` namespace
support is available. `SAFE_SANDBOX_ISOLATION=required` fails startup when it is
not available; `disabled` always uses direct process execution; `auto` falls
back to direct execution.

CPU, memory, and disk-write thresholds are monitored. Exceeding a configured
threshold requests a process kill. Failed processes restart up to five times by
default; override the count with `SAFE_SANDBOX_MAX_RESTARTS`.

Mode logs include stdout and stderr. Heartbeats are sampled every five seconds
and a connected mode becomes `unresponsive` after fifteen seconds without one.

## Telemetry Troubleshooting

Check adapter startup and received frames:

```bash
cargo run -p safectl -- logs --filter "telemetry adapter"
cargo run -p safectl -- get telemetry --tail 10
cargo run -p safectl -- watch telemetry
```

For `external` telemetry, verify that the configured shell command emits one
JSON object per line with a `payload` field. For source-sensitive modes, verify
that the frame's `source` exactly matches the mode profile.

## Mode Troubleshooting

```bash
cargo run -p safectl -- get modes --all --output json
cargo run -p safectl -- describe mode ExampleMode --output json
cargo run -p safectl -- logs --mode-name ExampleMode --follow
cargo run -p safectl -- send --op restart_mode --mode ExampleMode
```

Look for `starting`, `connecting`, `connected`, `disconnected`, `faulted`, and
`unresponsive` states. A mode that cannot connect may have an invalid binary
path, a protocol-version mismatch, a missing executable dependency, or a
sandbox isolation failure.

## Safety Limitations

The repository is not yet a flight-ready host command integration. Before
deployment, provide an external gatekeeper, verify command egress semantics,
reconcile metrics paths, enforce active-mode output filtering, and test recovery
with the target filesystem and process isolation policy.
