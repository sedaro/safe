# Runtime Configuration

SAFE loads a YAML runtime configuration and a separate JSON autonomy-mode
configuration. The YAML file controls the daemon, adapters, limits, and base
directories. The JSON file controls the mode processes and activation rules.

## Configuration Paths

The runtime YAML path is resolved in this order:

1. `SAFE_RUNTIME_CONFIG`.
2. `SAFE_RUNTIME_CONFIG_PATH`.
3. `safe/safe.yaml` relative to the current working directory, if it exists.
4. `/opt/safe/safe.yaml`.

`SAFE_RUNTIME_CONFIG` takes precedence when both environment variables are set.

The autonomy-mode JSON path is resolved in this order:

1. `SAFE_AUTONOMY_MODE_CONFIG_PATH`.
2. `safe/autonomy_mode_config.json` relative to the current working directory,
   if it exists.
3. `/opt/safe/autonomy_mode_config.json`.

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

base_paths:
  base_working_directory: "/opt/safe"
  base_writable_directory: "/tmp/safe"

platform:
  telemetry_adapter: "example"
  command_adapter: "safectl_unix_json"
  gatekeeper_adapter: "disabled"
  bash_mock_telemetry_command: null
  external_telemetry_command: null
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
| `logging.rotation.*` | `100`, `10`, `false` | Validated values, but file rotation is not currently implemented. |
| `limits.max_autonomy_modes` | `10` | Maximum number of JSON mode entries, including disabled entries. |
| `base_paths.base_working_directory` | `/opt/safe` | Deployment path retained by the config model; mode work directories currently derive from writable state. |
| `base_paths.base_writable_directory` | `/tmp/safe` | Root for SAFE state and output files. |
| `platform.telemetry_adapter` | `example` | Selects `example`, `bash_mock`, or `external`. |
| `platform.command_adapter` | `safectl_unix_json` | The currently supported command ingress/egress adapter. |
| `platform.gatekeeper_adapter` | `disabled` | Selects `disabled` or `external`. Disabled automatically approves batches. |
| `platform.bash_mock_telemetry_command` | none | Command used by `bash_mock`; the feature must be enabled at build time. |
| `platform.external_telemetry_command` | none | Shell command whose stdout supplies telemetry JSONL. |
| `platform.external_gatekeeper_command` | none | Shell command implementing the external gatekeeper JSONL protocol. |
| `gatekeeper` | `{}` | Arbitrary JSON passed to an external gatekeeper as an environment variable. |

SAFE validates that `max_autonomy_modes`, `rotation.max_files`, and
`rotation.max_file_size_mb` are greater than zero. It does not validate that
configured adapter commands or executable paths exist until they are started.

## Environment Overrides

Figment applies `SAFE_` environment variables after YAML and splits nested
fields on `__`. For example:

```bash
SAFE_PLATFORM__TELEMETRY_ADAPTER=external
SAFE_PLATFORM__EXTERNAL_TELEMETRY_COMMAND='cat /var/run/telemetry.jsonl'
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

`example` emits one frame per second with source `example` and a counter under
`payload.telemetry.temperature_value_c`.

`bash_mock` executes the configured shell command and interprets each non-empty
stdout line as a JSON payload. It assigns source `bash_mock` and uses a payload
`ts_mono` field when present, otherwise a local sequence number. Build SAFE with
the `platform-bash-mock` feature to enable this adapter.

`external` executes the configured command through `bash -lc`. Each non-empty
stdout line must be a JSON object with optional `source` and `ts_mono` fields and
a `payload` field. Invalid lines are logged and skipped.

## Command and Gatekeeper Adapters

The default `safectl_unix_json` adapter listens at:

```text
<base_paths.base_writable_directory>/state/safectl.sock
```

It accepts newline-delimited JSON command or telemetry ingress messages. See
[`safectl.md`](./safectl.md) for the wire shapes.

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
