# Anomaly Recovery

## Thermal Assessment User Story

The intended thermal-recovery workflow uses an LLM to assess telemetry,
command-board context, and simulation evidence together to judge whether a
thermal anomaly is occurring and whether a recovery proposal is warranted.
See [the thermal anomaly recovery user story](./thermal-anomaly-recovery-story.md)
for the target workflow, acceptance criteria, and gaps in the current branch.
The [implementation plan](./thermal-anomaly-recovery-plan.md) breaks this work
into ordered milestones with file-level changes and verification criteria.

## Current Implementation

`mode-anomaly-recovery` is an out-of-process SAFE autonomy mode. It evaluates
configured static nominal profiles locally as investigation triggers, then
records the latest host-owned telemetry and command-board evidence before it
asks the LLM to complete a thermal assessment. Completion records `thermal_anomaly`,
`no_thermal_anomaly`, or `inconclusive`; a recovery action is optional and may
only follow an anomaly assessment requesting recovery evaluation.

Evidence is retained in a bounded, host-owned ledger across tool calls. Telemetry
history is source-scoped and ignores duplicate or out-of-order timestamps for
trend/persistence use. Board proposals and approvals are command intent, not
execution acknowledgement. A telemetry or board update cancels pending work
before it can submit a stale proposal.

The EDS is intentionally used only for power and command-side-effect viability;
it does not contain a thermal model and is not thermal evidence. Thermal
assessment is based on telemetry, trends, board state, and LLM reasoning.
Missing thermal EDS outputs therefore do not block anomaly assessment or
power-only command viability.

### Post-selection viability

Recovery selection is only an assessment disposition. Before SAFE receives a
command, host code requires an exact action-specific recovery scenario and its
baseline association, then runs both from the same frozen evidence revision,
state bindings, and horizon. Both runs must succeed and return finite,
unit-declared `final_state_of_charge` and `minimum_state_of_charge` metrics;
the recovery contract also supplies machine-checkable minimum-SOC and maximum
degradation constraints. Missing metrics, failed or timed-out runs, mismatched
actions, stale revisions, and board duplicates/conflicts block output.

Power-only viability can support a short-term command decision, but it always
logs thermal benefit as unverified. Any future simulation-backed thermal claim
would require a separate thermal model and verified output units. Model-specific
EDS paths, IDs, and field names belong only in deployment configuration, never
in generic source or committed fixtures.

Profiles are selected by an exact `TelemetryFrame.source` match. Rule paths are
dot-separated and relative to `TelemetryFrame.payload`; numeric path segments
address array indexes.

The profile object is the advisor mode's `mode_config` value. The mode is
started by SAFE with the launch contract documented in
[`../safe/docs/mode-development.md`](../safe/docs/mode-development.md).

## Configuration

```json
{
  "llm": {
    "adapter": {
      "kind": "ollama",
      "config": {
        "endpoint": "http://127.0.0.1:11434/api/generate"
      }
    },
    "model": "mistral:7b"
  },
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
in diagnostics, but cannot emit a command. An empty action catalog is valid for
assessment-only operation; rules that name an action still require it to be
defined.

## LLM Adapters

`llm` is required. It selects a compiled-in adapter and defines model-level
generation settings:

| Field | Default |
| --- | --- |
| `llm.enable_tool_calls` | `true` |
| `llm.request_timeout_ms` | `20000` |
| `llm.response_temperature` | `0.0` |
| `llm.max_output_tokens` | `256` |
| `llm.context_window_tokens` | `2048` |
| `llm.context_safety_margin_tokens` | `256` |
| `max_prompt_chars` | `1600` |
| `max_response_chars` | `800` |
| `max_decision_attempts` | `3` |
| `max_feedback_chars` | `400` |
| `require_board_snapshot` | `false` |
| `decision_trace` | `false` |

The supported adapter kinds are:

| Kind | Provider config |
| --- | --- |
| `ollama` | `endpoint`: absolute Ollama generate URL, usually `http://127.0.0.1:11434/api/generate`. |
| `openai_compatible` | `endpoint`: absolute chat-completions URL. `api_key_env` is optional and names the environment variable containing the bearer token. HTTPS requires the `https` Cargo feature. |

For example, an OpenAI-compatible service can be configured without storing a
secret in `mode_config`:

```json
{
  "llm": {
    "adapter": {
      "kind": "openai_compatible",
      "config": {
        "endpoint": "https://api.example.com/v1/chat/completions",
        "api_key_env": "LLM_API_KEY"
      }
    },
    "model": "example-model",
    "request_timeout_ms": 20000,
    "response_temperature": 0.0,
    "max_output_tokens": 256,
    "context_window_tokens": 2048,
    "context_safety_margin_tokens": 256
  }
}
```

The default build contains only the adapter's small HTTP/1.1 client. Build with
`cargo build -p mode-anomaly-recovery --features https` to add native-root TLS
support for hosted endpoints such as the example above. The focused client does
not follow redirects; configure the final HTTP or HTTPS endpoint directly.

The adapter validates its own `config` object and rejects unknown provider
fields. `ollama_host`, `ollama_port`, `ollama_path`, top-level `model`, and
`num_predict` are no longer accepted. Migrate them to the `llm` block, using a
full Ollama endpoint and `max_output_tokens`.

Set `llm.enable_tool_calls` to `false` when the configured model or server does
not support native tool calls:

```json
{
  "llm": {
    "adapter": {
      "kind": "ollama",
      "config": {"endpoint": "http://127.0.0.1:11434/api/generate"}
    },
    "model": "mistral:7b",
    "enable_tool_calls": false
  }
}
```

In this mode the adapter uses its normal completion endpoint with JSON-object
output (`response_format.type=json_object` for OpenAI-compatible servers and
`format=json` for Ollama). Each planner phase requests strict JSON arguments for
its one available operation, then feeds the parsed result through the same
host-owned assessment, selection, simulation, staleness, and board-conflict
validation as a native tool call. Malformed, truncated, ambiguous, or oversized
textual responses fail closed.

The `safe-llm-adapter` crate exposes `LlmAdapter`, `LlmAdapterFactory`, and
`AdapterRegistry` for mission-specific Rust adapters. Custom adapters must be
linked into the mode binary and registered at startup; runtime shared-library
loading is not supported.

`goal` and `analysis_instructions` also have safe default text and may be
overridden to constrain the decision prompt.

`context_window_tokens` is the server's total context window, not a completion
allowance. Before every native-tool request, the mode conservatively estimates
the serialized messages and tools, adds `max_output_tokens` and the safety
margin, and fails closed if the total exceeds that window. Context-only tools
use at most 128 output tokens; assessment and selection use the configured
allowance. For a 2048-token server, keep the defaults unless the server's
chat-template overhead has been measured.

Prompts contain only phase-relevant candidate IDs, observations, action IDs,
and bounded evidence summaries. Assessment rationale and uncertainty are
bounded to 400 and 160 characters, up to two 100-character forecast risks are
allowed, and recovery-selection rationale is bounded to 200 characters. Host
validation enforces these limits even when a provider ignores JSON Schema.

## Live Decision Trace

For a terminal demo, set `decision_trace` to `true` in the mode's
`mode_config`. The advisor emits a compact, ordered `LLM DEMO` trace for the
configured candidates, each adapter request, the model's selected action and
rationale, validation or repair attempts, and the command-board proposal. The
trace is an auditable decision summary, not hidden model chain-of-thought.

Use at least two actionable choices to exercise the adapter path. A single
candidate with a single eligible action deliberately skips the model and the
trace says so. This `mode_config` is a compact local-demo example:

```json
{
  "llm": {
    "adapter": {
      "kind": "ollama",
      "config": {"endpoint": "http://127.0.0.1:11434/api/generate"}
    },
    "model": "mistral:7b"
  },
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

Place that object inside an enabled outer mode entry named `AnomalyRecoveryDemo`
(with `bin_path` set to `../target/debug/mode_anomaly_recovery`), build it, and run
SAFE as usual. In a second terminal, follow only the trace:

```bash
cargo build -p mode-anomaly-recovery
cargo run -p safectl -- logs --mode-name AnomalyRecoveryDemo --follow --filter "LLM DEMO"
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
LLM DEMO | attempt 1/3 | asking mistral:7b via ollama to select one action from 2 configured candidate(s)
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
| One actionable candidate with one eligible action | Emit that action deterministically. The LLM is not contacted. |
| Multiple actionable candidates or one candidate with multiple actions | Ask the configured adapter to select one configured candidate and action. |

The same candidate set is not planned repeatedly until its signature changes.
When `require_board_snapshot` is true, planning waits for the first board
snapshot from SAFE. Telemetry is evaluated while the mode is inactive, but
commands are planned only while the mode is active.

Each candidate includes a canonical `anomaly_id` formed as
`<profile_id>-<rule_id>`. LLM decisions must return that exact value. Bare rule
IDs remain accepted for compatibility, but the advisor prompt directs the model
to use the canonical scoped ID.

## Decision Transport

Each adapter receives the prompt, the strict decision JSON schema, model,
temperature, output-token limit, and request timeout. Ollama translates this to
`/api/generate`; the OpenAI-compatible adapter translates it to chat
completions with strict JSON-schema response formatting.

- `model`, chat `messages`, native `tools`, and `stream: false`.
- `run_eds_simulation`, which accepts only a configured scenario ID and its
  configured bounded numeric parameters.
- `select_recovery_action`, which may choose only a frozen candidate and one of
  its eligible configured actions. Evidence is derived from that candidate.
- Telemetry and command-board snapshots are host-collected before the first
  model request and supplied as compact evidence summaries. They are not
  model-invoked tools, avoiding a native-tool round trip for data the mode
  already owns.
- `options.temperature` and `options.num_predict`.

When `llm.enable_tool_calls` is `true`, the configured model must support native
tool calls. Unsupported tools, parallel calls, extra or malformed arguments,
HTTP failures, oversized payloads, timeouts, and exhausted turn/run budgets fail
safely without a command.

## Local EDS Scenarios

`simulation` is optional. It names one trusted local `eds_path` and allow-listed
scenarios. A scenario declares applicable nominal-rule IDs, allowed actions,
duration, trusted constant or telemetry-derived patch bindings, optional bounded
parameters, and compact numeric output metrics. The model never receives EDS
paths, patches, raw frames, stdout, stderr, shell arguments, or filesystem paths.
Each call creates an independent `SedaroSimulator` run. There is no cloud API.

The adapter returns only normalized completion text and finish status. The mode
then enforces the response size, strict JSON parsing, selected anomaly ID,
eligible action, and exact evidence path. HTTP errors, timeouts, malformed
responses, token-limit truncation, empty responses, oversized responses, and
validation failures are retried up to `max_decision_attempts`. Parse and
validation failures include bounded repair feedback.

The telemetry and board tools are read-only. Their responses are bounded to
2,000 characters and report `unavailable` until SAFE has broadcast the
corresponding snapshot. They read the newest snapshot available at invocation
time, but do not change the frozen candidate/action allow-list or bypass SAFE
board and gatekeeper validation.

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
    "name": "AnomalyRecoveryExample",
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
      "llm": {
        "adapter": {
          "kind": "ollama",
          "config": {"endpoint": "http://127.0.0.1:11434/api/generate"}
        },
        "model": "mistral:7b"
      },
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

The opt-in live example uses the OpenAI-compatible endpoint and model from
`safe/autonomy_mode_config.json`, sends fake high-temperature telemetry through
the real SAFE mode transport, and runs the configured real EDS twice for the
baseline/recovery viability check. It requires `OPENAI_API_KEY` and the
configured EDS workspace (the checked-in example uses `/workspace/bundle_juno`):

```bash
cargo test -p mode-anomaly-recovery --features https \
  --test live_openai_simulation_e2e \
  -- --ignored --nocapture
```

The test captures mode logs and verifies thermal assessment, post-selection
simulation, power-only thermal separation, and command proposal stages. It is
ignored by default because it consumes OpenAI API and EDS resources.
