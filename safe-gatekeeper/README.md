# `safe-gatekeeper`

`safe-gatekeeper` evaluates proposed command batches by running a configured
SCF EDS and checking numeric fields in its outputs. It contains no assumptions
about a particular spacecraft, telemetry schema, command projection, or EDS
field layout.

## Lifecycle

1. SAFE sends normalized telemetry frames to the gatekeeper. The gatekeeper
   retains the latest frame without decoding its payload.
2. SAFE sends an evaluation request containing the command board and candidate
   command IDs.
3. The gatekeeper starts `input_adapter_command` as a one-shot child process.
4. The gatekeeper writes a `SimulationInputRequest` JSON object to the child's
   stdin and closes stdin.
5. The adapter interprets the telemetry and board using mission-specific logic,
   then writes one `SimulationInputResponse` JSON object to stdout.
6. The gatekeeper applies the returned start epoch and patches, runs the EDS for
   `sim_duration_days`, and collects its outputs.
7. Every configured field check must pass for the batch to be approved.

The adapter must write logs to stderr because stdout is reserved for its JSON
response. A missing telemetry frame, adapter error, malformed response,
simulation error, missing checked field, or failed check rejects the batch.

## Configuration

SAFE passes the `gatekeeper` object from `safe.yaml` through the
`SAFE_GATEKEEPER_CONFIG_JSON` environment variable:

```yaml
platform:
  gatekeeper_adapter: external
  external_gatekeeper_command: /opt/safe/bin/safe_gatekeeperd

gatekeeper:
  eds_path: /opt/safe/eds
  sim_duration_days: 1.0
  input_adapter_command:
    - /opt/safe/bin/mission-input-adapter
    - gatekeeper-input
  input_adapter_config:
    deployment: example
  field_checks:
    - target_file: agent.engine.jsonl
      field: component.metric
      aggregation: min
      op: gte
      threshold: 0.5
```

`input_adapter_config` is optional opaque JSON forwarded to the adapter. It
allows mission deployments to change adapter behavior without adding
mission-specific options to the gatekeeper.

## Adapter Contract

The one-shot adapter receives:

```json
{
   "telemetry": {"source":"example-source","ts_mono":42,"payload":"{}"},
  "board": {"proposals":{},"rejected":{},"approved":{},"source_of_truth":[]},
  "candidate_command_ids": [],
   "config": {"deployment":"example"}
}
```

It returns an MJD epoch and the generic `safe_sim::EdsPatch` values to apply:

```json
{
  "start_time_mjd": 60000.0,
  "patches": [
    {
      "agent_id": "agent-id",
      "engine": "engine-name",
      "field": "component-id.field-name",
      "type_": "f64",
      "value": "1.0"
    }
  ]
}
```

The adapter owns all mission-specific IDs, telemetry extraction, unit
conversion, epoch derivation, and board-command schedule projection.
