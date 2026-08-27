# Runtime Configuration

SAFE loads a YAML runtime configuration and a separate JSON autonomy-mode
configuration. The YAML file controls the daemon, adapters, limits, and base
directories. The JSON file controls the mode processes and activation rules.

## Configuration Paths

The runtime YAML path is resolved in this order:

1. `SAFE_RUNTIME_CONFIG`.
2. `SAFE_RUNTIME_CONFIG_PATH`.
3. `safe/safe.yaml` relative to the current working directory, if it exists.
4. `/tmp/safe/safe.yaml`.

`SAFE_RUNTIME_CONFIG` takes precedence when both environment variables are set.

The autonomy-mode JSON path is resolved in this order:

1. `SAFE_AUTONOMY_MODE_CONFIG_PATH`.
2. `safe/autonomy_mode_config.json` relative to the current working directory,
   if it exists.
3. `/tmp/safe/autonomy_mode_config.json`.

Run SAFE from the repository root or use explicit paths. The repository
[`run.sh`](../../run.sh) sets both paths for local development.

## YAML Schema

All of the following top-level sections have defaults, but a deployment should
keep the complete structure visible in its checked-in configuration:

```yaml
tracing:
  level: "info"
  filter: "info"
  with_target: true

sockets:
  telemetry:
    ip: "127.0.0.1"
    port: 44212
  commands:
    ip: "127.0.0.1"
    port: 7002

logging:
  file_path: "/tmp/safe/logs/safe.log"
  rotation:
    max_file_size_mb: 200
    max_files: 7
    daily: true

limits:
  max_autonomy_modes: 10

persistence:
  events_max_bytes: 16777216
  events_max_records: 10000
  outputs_max_bytes: 16777216
  outputs_max_records: 10000

base_paths:
  base_working_directory: "/tmp/safe"
  base_writable_directory: "/tmp/safe"

platform:
  telemetry_adapter: "example"
  command_adapter: "safectl_unix_json"
  egress_adapter: "safectl_filesystem"
  gatekeeper_adapter: "disabled"
  external_telemetry_command: null
  external_egress_command: null
  external_egress_retry:
    initial_delay_ms: 100
    max_delay_ms: 30000
    stable_session_ms: 30000
    write_timeout_ms: 5000
  external_gatekeeper_command: null

gatekeeper: {}
```

## Fields and Defaults

| Field | Default | Description |
| --- | --- | --- |
| `tracing.level` | `info` | Fallback tracing filter. |
| `tracing.filter` | `info` | Primary tracing-subscriber filter. |
| `tracing.with_target` | `true` | Include Rust targets in log files. |
| `sockets.telemetry` | `0.0.0.0:44212` | Retained socket configuration; the current adapters do not bind this endpoint. |
| `sockets.commands` | `127.0.0.1:7002` | Retained socket configuration; command ingress currently uses a Unix socket. |
| `logging.file_path` | `/tmp/safe/logs/app.log` | Its parent directory is used for `default.log` and per-mode logs. |
| `logging.rotation.max_file_size_mb` | `100` | Maximum bytes in one log file, in mebibytes. A record that exceeds this limit is dropped rather than exceeding it. |
| `logging.rotation.max_files` | `10` | Maximum retained files per stream, including the active file. Older numbered archives are removed. |
| `logging.rotation.daily` | `false` | Rotate a non-empty active log when its UTC calendar day changes. Size rotation always applies. |
| `limits.max_autonomy_modes` | `10` | Maximum number of JSON mode entries, including disabled entries. |
| `persistence.events_max_bytes` | `16777216` | Compact the event recovery journal after it reaches this byte count. |
| `persistence.events_max_records` | `10000` | Compact the event recovery journal after this many records. |
| `persistence.outputs_max_bytes` | `16777216` | Compact the output recovery journal after it reaches this byte count. |
| `persistence.outputs_max_records` | `10000` | Compact the output recovery journal after this many records. |
| `base_paths.base_working_directory` | `/tmp/safe` | Deployment path retained by the config model; mode work directories currently derive from writable state. |
| `base_paths.base_writable_directory` | `/tmp/safe` | Root for SAFE state and output files. |
| `platform.telemetry_adapter` | `example` | Selects `example` or `external`. |
| `platform.command_adapter` | `safectl_unix_json` | Selects the command ingress adapter. |
| `platform.egress_adapter` | `safectl_filesystem` | Selects `safectl_filesystem` or `external` platform egress. |
| `platform.gatekeeper_adapter` | `disabled` | Selects `disabled` or `external`. Disabled automatically approves batches. |
| `platform.external_telemetry_command` | none | Shell command whose stdout supplies telemetry JSONL. |
| `platform.external_egress_command` | none | Shell command implementing the external egress JSONL protocol. |
| `platform.external_egress_retry.initial_delay_ms` | `100` | Delay before the first external egress restart. |
| `platform.external_egress_retry.max_delay_ms` | `30000` | Maximum exponential restart delay. |
| `platform.external_egress_retry.stable_session_ms` | `30000` | Runtime after which a failed session resets the backoff to its initial delay. |
| `platform.external_egress_retry.write_timeout_ms` | `5000` | Maximum time allowed for one JSONL write to the child before restarting it. |
| `platform.external_gatekeeper_command` | none | Shell command implementing the external gatekeeper JSONL protocol. |
| `gatekeeper` | `{}` | Arbitrary JSON passed to an external gatekeeper as an environment variable. |

SAFE validates that `max_autonomy_modes`, all persistence limits, log rotation
limits, and external egress retry durations are greater than zero. The initial
egress retry delay must not exceed its maximum. SAFE also validates that the
configured log size can be represented in bytes. Each log stream retains at most
`rotation.max_files * rotation.max_file_size_mb` MiB. This bound is per stream,
not a global limit for the logging directory. It does not validate that
configured adapter commands or executable paths exist until they are started.

## Environment Overrides

Figment applies `SAFE_` environment variables after YAML and splits nested
fields on `__`. For example:

```bash
SAFE_PLATFORM__TELEMETRY_ADAPTER=external
SAFE_PLATFORM__EXTERNAL_TELEMETRY_COMMAND='cat /var/run/telemetry.jsonl'
SAFE_PLATFORM__EGRESS_ADAPTER=external
SAFE_PLATFORM__EXTERNAL_EGRESS_COMMAND='/usr/local/bin/platform-egress'
SAFE_BASE_PATHS__BASE_WRITABLE_DIRECTORY=/var/lib/safe
```

The configuration-path variables and mode/sandbox variables are read directly
by the runtime:

| Variable | Purpose |
| --- | --- |
| `SAFE_RUNTIME_CONFIG` | Highest-priority runtime YAML path. |
| `SAFE_RUNTIME_CONFIG_PATH` | Fallback runtime YAML path. |
| `SAFE_AUTONOMY_MODE_CONFIG_PATH` | Autonomy-mode JSON path. |
| `SAFE_MODE_ENDPOINT` | Mode IPC endpoint when a mode is run outside SAFE. |
| `SAFE_MODE_ID` | Mode UUID when a mode is run outside SAFE. |
| `SAFE_MODE_WORKING_DIRECTORY` | Mode working directory when not supplied as an argument. |
| `SAFE_SANDBOX_ISOLATION` | `auto`, `required`, or `disabled`; defaults to `auto`. |
| `SAFE_SANDBOX_MAX_RESTARTS` | Sandbox restart limit; defaults to `5`. |
| `SAFE_METRIC_BASE_PATH` | Base directory used by the sandbox metrics writer; defaults to `/tmp/safe`. |

`SAFE_USE_TELEM_TS_NOW` is not read by the current runtime and should not be
used as a supported configuration switch.

## Telemetry Adapters

`example` emits one frame per second with source `example` and counters under
`payload.telemetry.batt_v` and `payload.telemetry.batt_c`.

`external` executes the configured command through `bash -lc`. Each non-empty
stdout line must be a JSON object with optional `source` and `ts_mono` fields and
a `payload` field. Invalid lines are logged and skipped.

## Command, Egress, and Gatekeeper Adapters

The default `safectl_unix_json` adapter listens at:

```text
<base_paths.base_writable_directory>/state/safectl.sock
```

It accepts newline-delimited JSON command or telemetry ingress messages. See
[`safectl.md`](./safectl.md) for the wire shapes.

The default `safectl_filesystem` egress writes host command status records to
`state/host_command_status.jsonl` and approved scheduled commands to
`out/commands.csv`.

The `external` egress adapter executes `external_egress_command` through
`bash -lc`. SAFE writes JSONL messages to the child's stdin and reads JSONL
responses from stdout. SAFE restarts a failed, exited, or write-stalled child
indefinitely using the configured capped exponential backoff. A session that
runs for `stable_session_ms` resets the next delay to `initial_delay_ms`.
The child receives either a status update:

```json
{"kind":"host_command_status","status":{"request_id":"request-1","state":"accepted","detail":"command accepted","ts_mono":42}}
```

or a complete board snapshot:

```json
{"kind":"board_snapshot","board":{"proposals":{},"rejected":{},"approved":{},"source_of_truth":[]}}
```

After delivering commands from a board snapshot, the child must return:

```json
{"kind":"board_published","command_ids":["42:00000000-0000-0000-0000-000000000001:0"]}
```

SAFE marks only the acknowledged command IDs as `Published`. A successful
stdin write alone does not mark a command published.

An external egress may request that SAFE remove commands from the board by
writing this message to stdout:

```json
{"kind":"clear_board_commands","command_ids":["42:00000000-0000-0000-0000-000000000001:0"],"reason":"host schedule cleared"}
```

SAFE persists a cancellation event for each requested ID. These cancellations
are attributed to the platform egress actor
`ffffffff-ffff-ffff-ffff-ffffffffffff`; the gatekeeper remains the all-zero
actor. This only clears SAFE's board and does not itself cancel commands that
the host has already accepted.

This repository includes an example compatible executable. Build it with:

```bash
cargo build -p safe --bin platform-egress-example
```

It reproduces the filesystem adapter's status JSONL and scheduled-command CSV
outputs under its `--base-path` (default: `/tmp/safe`):

```yaml
platform:
  egress_adapter: external
  external_egress_command: "/path/to/platform-egress-example --base-path /tmp/safe"
  external_egress_retry:
    initial_delay_ms: 100
    max_delay_ms: 30000
    stable_session_ms: 30000
    write_timeout_ms: 5000
```

The disabled gatekeeper consumes pending-batch requests and immediately emits
approval. The external gatekeeper receives JSONL on stdin and must write JSONL
approval or rejection messages on stdout. SAFE sets `SAFE_GATEKEEPER_CONFIG_JSON`
to the configured `gatekeeper` value for the child process.

The external wire shapes are:

```json
{"kind":"telemetry","frame":{"source":"example","ts_mono":42,"payload":"{\"ok\":true}"}}
{"kind":"evaluate_batch","request_id":1,"board":{"proposals":{},"rejected":{},"approved":{},"source_of_truth":[]},"candidate_command_ids":["1:00000000-0000-0000-0000-000000000001:0"]}
```

The child returns one of these for an evaluation request:

```json
{"kind":"approve","request_id":1,"details":"approved"}
{"kind":"reject","request_id":1,"reason":"not safe"}
```

## Mode Configuration

The YAML file does not define mode binaries. Add mode entries to the separate
`autonomy_mode_config.json` file and use the activation reference guide:

[`activation-config.md`](./activation-config.md)
