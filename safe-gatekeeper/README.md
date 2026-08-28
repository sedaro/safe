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
6. The gatekeeper applies the returned start epoch and patches, runs the nominal
   EDS case for `sim_duration_days`, and collects its outputs.
7. When Monte Carlo analysis is configured, the gatekeeper varies configured
   scalar patches around the adapter values and runs each generated case.
8. The nominal case and the configured fraction of Monte Carlo cases must pass
   every field check for the batch to be approved.

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
  input_adapter_timeout_secs: 10
  input_adapter_config:
    deployment: example
  simulation_timeout_secs: 300
  field_checks:
    - target_file: agent.engine.jsonl
      field: component.metric
      aggregation: min
      op: gte
      threshold: 0.5
      tolerance: 0.000000001
  monte_carlo:
    samples: 50
    seed: 42
    minimum_pass_fraction: 1.0
    max_resample_attempts: 1000
    variations:
      - name: state_of_charge_error
        target:
          agent_id: agent-id
          engine: power
          field: component-id.state_of_charge
          type_: f64
        operation: add
        distribution:
          kind: normal
          mean: 0.0
          std_dev: 0.02
        bounds:
          min: 0.0
          max: 1.0
```

`input_adapter_config` is optional opaque JSON forwarded to the adapter. It
allows mission deployments to change adapter behavior without adding
mission-specific options to the gatekeeper.

`input_adapter_timeout_secs` limits each one-shot adapter invocation, while
`simulation_timeout_secs` limits each nominal or Monte Carlo EDS process. Both
must be greater than zero when supplied. Omitting either retains the previous
unlimited behavior.

`tolerance` is an optional non-negative absolute tolerance used by `eq` and
`ne` field checks; it defaults to `1e-9`. Ordered comparisons ignore it.

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

## Monte Carlo Analysis

Omit `monte_carlo` to run only the nominal simulation. When configured, the
gatekeeper always requires the nominal adapter-provided state to pass. The
nominal run is not included in `minimum_pass_fraction`, which defaults to `1.0`.
The seed defaults to `0`, and a variation's operation defaults to `replace`.

Each variation identifies an EDS field by `agent_id`, `engine`, and `field`.
The configured `type_` is still included in every EDS patch and must agree with
the adapter patch when that target already exists. This lookup rule makes a
type mismatch an explicit configuration error instead of sending two
conflicting patches for one logical field.

Scalar values can be varied with:

- `replace`: use the sampled value directly. This may create a patch not
  returned by the adapter, using the configured `type_`.
- `add`: add the sampled value to the adapter patch's numeric value.
- `multiply`: multiply the adapter patch's numeric value by the sampled value.

`add` and `multiply` require exactly one matching adapter patch. `replace` can
target an absent adapter patch, but EDS remains responsible for validating that
the configured field and type exist. An unsupported patch causes EDS to exit
unsuccessfully and the entire gatekeeper evaluation is rejected.

Supported distribution kinds are `normal`, `uniform`, `log_normal`,
`triangular`, and `discrete`. Optional inclusive `bounds` apply to the final
value after its operation. Out-of-bounds values are resampled up to the limit
configured by `max_resample_attempts`, which defaults to `1000` and must
be greater than zero. An impossible bounded distribution rejects the
evaluation.

Sampling is reproducible: the same seed, ordered variations, distributions,
and sample count produce the same random perturbations. Current telemetry can
still change the final values produced by `add` and `multiply` because their
baseline comes from the adapter on each evaluation.

Only completed simulations that violate field checks count as failed Monte
Carlo cases. Adapter failures, invalid patches, EDS non-zero exits, missing
outputs, and output decoding failures reject the entire evaluation regardless
of `minimum_pass_fraction`.
