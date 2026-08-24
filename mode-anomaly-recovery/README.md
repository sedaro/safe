# LLM Advisor Static Nominal Profiles

`mode-anomaly-recovery` is an out-of-process SAFE autonomy mode. It evaluates
configured static nominal profiles locally and emits profile-backed commands.
It contacts Ollama only when local evaluation leaves more than one actionable
choice.

Profiles are selected by an exact `TelemetryFrame.source` match. Rule paths are
dot-separated and relative to `TelemetryFrame.payload`; numeric path segments
address array indexes.

The profile object is the advisor mode's `mode_config` value. The mode is
started by SAFE with the launch contract documented in
[`../safe/docs/mode-development.md`](../safe/docs/mode-development.md).

## Configuration

```json
{
  "action_catalog": [
    {
      "id": "point_sun_yaw",
      "description": "Point solar arrays toward the sun.",
      "preconditions": ["Attitude control is available."]
    }
  ],
  "nominal_profiles": [
    {
      "id": "example-v1",
      "source": "example",
      "rules": [
        {
          "id": "temperature_out_of_nominal",
          "path": "telemetry.temperature_c",
          "kind": "number_range",
          "min": -20.0,
          "max": 45.0,
          "min_consecutive_samples": 2,
          "severity": "high",
          "eligible_actions": ["point_sun_yaw"]
        }
      ]
    }
  ]
}
```

`nominal_profiles` must contain at least one profile. Each profile has a unique
`id`, a unique exact-match `source`, and at least one rule. Rule IDs are unique
across the whole advisor configuration. Unknown JSON fields are rejected.

Supported rule kinds are:

| Kind | Fields | Violation |
| --- | --- | --- |
| `number_range` | `min`, `max`, or both | Observed number is outside the inclusive range. |
| `enum` | Non-empty `allowed` strings | Observed string is not in `allowed`. |
| `boolean` | `expected` | Observed boolean differs from `expected`. |
| `required` | No expectation fields | Value is missing or null. |

Every rule defaults `min_consecutive_samples` to `1` and `severity` to
`medium`. Missing or type-invalid fields reset the rule episode and never emit
a command. A violation becomes a candidate after the configured consecutive
sample count is reached.

The action catalog may contain only these recommendable actions:

| Action ID | SAFE command |
| --- | --- |
| `point_sun_yaw` | `Command::PointSunYaw` |
| `point_nadir` | `Command::PointNadir` |
| `thruster_off` | `Command::ThrusterOff` |

`capture_image` and `noop` are representable enum values but are rejected for
recommendations. A rule with no `eligible_actions` is observable and can appear
in diagnostics, but cannot emit a command. An action catalog is required when
any rule has eligible actions; actionless schema profiles are valid.

## Defaults

| Field | Default |
| --- | --- |
| `ollama_host` | `127.0.0.1` |
| `ollama_port` | `11434` |
| `ollama_path` | `/api/generate` |
| `model` | `mistral:7b` |
| `request_timeout_ms` | `10000` |
| `max_prompt_chars` | `3500` |
| `max_response_chars` | `800` |
| `response_temperature` | `0.0` |
| `num_predict` | `256` |
| `max_decision_attempts` | `3` |
| `max_feedback_chars` | `400` |
| `require_board_snapshot` | `false` |
| `decision_trace` | `false` |

`goal` and `analysis_instructions` also have safe default text and may be
overridden to constrain the decision prompt.

## Live Decision Trace

For a terminal demo, set `decision_trace` to `true` in the mode's
`mode_config`. The advisor emits a compact, ordered `LLM DEMO` trace for the
configured candidates, each Ollama request, the model's selected action and
rationale, validation or repair attempts, and the command-board proposal. The
trace is an auditable decision summary, not hidden model chain-of-thought.

Use at least two actionable choices to exercise the Ollama path. A single
candidate with a single eligible action deliberately skips the model and the
trace says so. This `mode_config` is a compact local-demo example:

```json
{
  "decision_trace": true,
  "action_catalog": [
    {"id": "point_sun_yaw", "description": "Point solar arrays toward the sun."},
    {"id": "point_nadir", "description": "Point the payload toward nadir."}
  ],
  "nominal_profiles": [
    {
      "id": "demo-v1",
      "source": "demo",
      "rules": [
        {
          "id": "temperature_high",
          "path": "telemetry.temperature_c",
          "kind": "number_range",
          "max": 45.0,
          "severity": "high",
          "eligible_actions": ["point_sun_yaw"]
        },
        {
          "id": "mode_invalid",
          "path": "telemetry.mode",
          "kind": "enum",
          "allowed": ["idle", "nominal"],
          "severity": "medium",
          "eligible_actions": ["point_nadir", "point_sun_yaw"]
        }
      ]
    }
  ]
}
```

Place that object inside an enabled outer mode entry named `LlmAdvisorDemo`
(with `bin_path` set to `../target/debug/mode_anomaly_recovery`), build it, and run
SAFE as usual. In a second terminal, follow only the trace:

```bash
cargo build -p mode-anomaly-recovery
cargo run -p safectl -- logs --mode-name LlmAdvisorDemo --follow --filter "LLM DEMO"
```

Then send a source-bearing frame that violates both rules:

```bash
cargo run -p safectl -- send telemetry --json '{"source":"demo","ts_mono":1,"payload":{"telemetry":{"temperature_c":52.0,"mode":"fault"}}}'
```

With the mode active, this produces candidate, request, response, validation,
and proposal lines in order, for example:

```text
LLM DEMO | 2 detected candidate(s); 2 have configured actions
LLM DEMO | demo-v1-temperature_high | telemetry.temperature_c=52.0 | expected at most 45 | actions: point_sun_yaw
LLM DEMO | attempt 1/3 | asking mistral:7b to select one action from 2 configured candidate(s)
LLM DEMO | attempt 1/3 | model selected demo-v1-temperature_high -> point_sun_yaw | rationale: Temperature is above the configured limit.
LLM DEMO | attempt 1/3 | accepted demo-v1-temperature_high -> point_sun_yaw; evidence path is allowed
LLM DEMO | submitted point_sun_yaw for demo-v1-temperature_high (telemetry.temperature_c) to the SAFE command board
```

Use this only in controlled demos because it retains candidate values and the
model's stated rationale in the mode log.

## Decision Behavior

The advisor uses this decision matrix:

| Situation | Result |
| --- | --- |
| No matching profile, missing field, invalid type, or normal telemetry | No candidate and no command. |
| Candidates exist but none have eligible actions | No command. |
| One actionable candidate with one eligible action | Emit that action deterministically. Ollama is not contacted. |
| Multiple actionable candidates or one candidate with multiple actions | Ask Ollama to select one configured candidate and action. |

The same candidate set is not planned repeatedly until its signature changes.
When `require_board_snapshot` is true, planning waits for the first board
snapshot from SAFE. Telemetry is evaluated while the mode is inactive, but
commands are planned only while the mode is active.

Each candidate includes a canonical `anomaly_id` formed as
`<profile_id>-<rule_id>`. LLM decisions must return that exact value. Bare rule
IDs remain accepted for compatibility, but the advisor prompt directs the model
to use the canonical scoped ID.

## Ollama Integration

The advisor sends a plain HTTP `POST` to
`http://<ollama_host>:<ollama_port><ollama_path>` with a JSON body containing:

- `model`, `prompt`, and `stream: false`.
- A strict JSON response schema requiring `anomaly_id`, `action_id`, `reason`,
  and `evidence_paths`.
- `options.temperature` and `options.num_predict`.

The response must contain a non-empty `response` string containing strict JSON.
The selected anomaly ID, action ID, and evidence path must exactly match the
configured candidates. HTTP errors, timeouts, malformed JSON, token-limit
truncation, empty responses, oversized responses, and validation failures are
retried up to `max_decision_attempts`. Parse and validation failures include a
bounded repair-feedback prompt.

## SAFE Integration

The advisor emits:

```text
TimedCommand::Now(Command::<configured action>)
```

to SAFE as a board proposal. Emitting a command means submitting it to SAFE;
it does not by itself mean that a host vehicle executed it. Gatekeeper and
platform egress behavior is described in
[`../safe/docs/runtime-operations.md`](../safe/docs/runtime-operations.md).

A minimal outer SAFE mode entry is:

```json
[
  {
    "name": "LlmAdvisorExample",
    "priority": 10,
    "enabled": false,
    "bin_path": "../target/debug/mode_anomaly_recovery",
    "args": [],
    "sandbox_resources": {
      "cpu": 25.0,
      "memory": 536870912,
      "disk": 104857600
    },
    "persist_work_dir": true,
    "mode_config": {
      "action_catalog": [
        {
          "id": "point_sun_yaw",
          "description": "Point solar arrays toward the sun."
        }
      ],
      "nominal_profiles": [
        {
          "id": "example-v1",
          "source": "example",
          "rules": [
            {
              "id": "temperature_out_of_nominal",
              "path": "telemetry.temperature_c",
              "kind": "number_range",
              "min": -20.0,
              "max": 45.0,
              "min_consecutive_samples": 2,
              "eligible_actions": ["point_sun_yaw"]
            }
          ]
        }
      ]
    }
  }
]
```

The path is relative to `safe/autonomy_mode_config.json` when that is the
configuration file. Build the binary first with
`cargo build -p mode-anomaly-recovery`. The example is disabled intentionally; enable
it only after configuring a gatekeeper and mission-approved limits.

For a source-bearing telemetry frame through `safectl`, use a direct telemetry
frame with a decoded payload object:

```bash
cargo run -p safectl -- send telemetry --json '{"source":"example","ts_mono":42,"payload":{"telemetry":{"temperature_c":20.0}}}'
```

The external telemetry adapter uses the same decoded object shape. Full ingress
JSON with a string-encoded payload remains supported for compatibility.

## Fixtures and Tests

The generic fixtures are:

- [`testdata/static_nominal_profile.json`](./testdata/static_nominal_profile.json)
- [`testdata/static_nominal_telemetry.jsonl`](./testdata/static_nominal_telemetry.jsonl)

Run the advisor unit and integration tests with:

```bash
cargo test -p mode-anomaly-recovery
```
