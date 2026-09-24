# Anomaly Recovery

## Deterministic reboot and cooldown

The mode supports a deterministic recovery procedure selected by
`mode_config.recovery`: critical low power or high temperature → one durable
shutdown attempt → reboot-persistent 100-minute minimum wait → fresh recovery
threshold checks → `NOOP` handoff using existing SAFE activation rules.

See [deterministic recovery](./deterministic-recovery.md) for the configuration,
mode-local state machine, routing example, optional LLM advisor, and FlatSat
verification steps. The procedure runs without an LLM or EDS. The assessment
workflow below remains available when `recovery` is omitted.

The [synthetic recovery/advisory configuration](./testdata/recovery_advisory_profile.json)
illustrates optional LLM analysis with paired power simulations. Supply deployment
telemetry bindings, model settings, and thresholds in your local configuration.

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

Recovery can issue a SAFE board command or execute the mode-local `shutdown`
action after a compute-on/compute-shutdown power comparison. Shutdown invokes
the configured Linux command directly from the anomaly mode (default:
`/sbin/shutdown -h now`).

Evidence is retained in a configurable bounded, host-owned ledger across tool calls. Telemetry
history is source-scoped and ignores duplicate or out-of-order timestamps for
trend/persistence use. Board proposals and approvals are command intent, not
execution acknowledgement. Telemetry received during planning is coalesced for
the next investigation. Board updates invalidate planning when
`replanning.replan_on_board_change` is enabled. Final staleness checks prevent a
proposal based on changed telemetry or board evidence.

The EDS is intentionally used only for power and command-side-effect viability;
it does not contain a thermal model and is not thermal evidence. Thermal
assessment is based on telemetry, trends, board state, and LLM reasoning.
Missing thermal EDS outputs therefore do not block anomaly assessment or
power-only command viability.

### Post-selection viability

Recovery selection is only an assessment disposition. Before a board command or
local shutdown is issued, mode code requires an exact action-specific recovery scenario and its
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
  "schema_version": 1,
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

| Action ID | Execution |
| --- | --- |
| `point_sun_yaw` | `Command::PointSunYaw` |
| `point_nadir` | `Command::PointNadir` |
| `thruster_off` | `Command::ThrusterOff` |
| `shutdown` | Mode-local `shutdown_command` (default `/sbin/shutdown -h now`), after paired power simulation |

`capture_image` and `noop` are representable enum values but are rejected for
recommendations. A rule with no `eligible_actions` is observable and can appear
in diagnostics, but cannot emit a command. An empty action catalog is valid for
assessment-only operation; rules that name an action still require it to be
defined.

### Local shutdown recovery

The [shutdown profile](./testdata/shutdown_profile.json) is a complete synthetic
`mode_config` example. The anomaly entry in
[`../safe/autonomy_mode_config.json`](../safe/autonomy_mode_config.json) also uses
shutdown for high-temperature recovery. Replace example EDS paths, agent/field
names, load values and policy thresholds with deployment values before use.

The sequence is thermal assessment → eligible `shutdown` selection → paired
power simulation → final lifecycle/evidence checks → local Linux shutdown.
Shutdown is not a SAFE board proposal and does not go through gatekeeper approval
or platform egress. Board context is still required: existing proposals,
approvals or source-of-truth commands block the simulation because their effects
are not projected into either run.

Set `shutdown_command` inside `mode_config` to an array containing the executable
followed by its arguments. Omitting it preserves the default:

```json
{
  "shutdown_command": ["/sbin/shutdown", "-h", "now"]
}
```

For a deployment-specific helper, for example:

```json
{
  "shutdown_command": ["/usr/local/sbin/compute-poweroff", "--reason", "thermal anomaly"]
}
```

Arguments are passed literally, including spaces; there is no implicit shell
parsing, expansion or quoting. A bare executable name uses the mode process's
`PATH`; relative paths are relative to its process working directory. The array
must contain a nonblank executable and only strings, with no NUL characters.
The command is deployment configuration and is not supplied to or editable by
the LLM. Updating the configuration changes subsequent invocations without
clearing the attempt latch or journal.

Configure `simulation.initialization.compute_power_bindings`, for example:

```json
{
  "compute_power_bindings": [
    {
      "id": "compute_load",
      "agent_id": "spacecraft",
      "engine": "power",
      "field": "compute.power",
      "operating_power_w": 12.0,
      "shutdown_power_w": 0.5
    }
  ]
}
```

These numbers and identifiers are **synthetic examples**, not measured hardware
values. The target must be an executable `f64` EDS load field in watts. Supply
measured operating and residual shutdown draw; shutdown draw must be nonnegative
and lower than operating draw. The binding describes a constant load over the
simulation horizon, with shutdown applied at the start; it does not model Linux
shutdown latency or transients. A shutdown-state load need not be zero.

The recovery scenario uses `modeled_action: "shutdown"`,
`compute_power_binding: "compute_load"`, `role: "recovery"`, `thermal: false`,
and an exact `baseline_scenario_id`. It does not use `command_schedule_binding`.
The baseline uses operating draw; recovery uses shutdown draw. Shared state,
epoch, other loads and horizon are identical. A compute-only initialization may
omit `command_schedules`. Model schedules that could override the configured
load must be cleared through shared initialization patches.

Both runs must pass the existing final/minimum SOC and degradation checks with
finite, consistent-unit metrics. Missing initialization, wrong bindings,
simulation failure/timeout, missing metrics or stale evidence prevent shutdown.
Power viability does not predict cooling; thermal justification comes from the
assessment. New telemetry coalesced during planning also blocks local execution
until a fresh investigation validates it.

**Linux deployment:** the mode must run with permission to shut down the host,
and the configured executable must be available. The current SAFE namespace launcher uses a new
PID namespace. Launch SAFE with `SAFE_SANDBOX_ISOLATION=disabled` on the target
host for this action, and use a service identity authorized for host shutdown.
That setting applies to all modes in this SAFE instance. Running SAFE inside a
container does not establish host shutdown access. The mode runs exactly the
configured executable and arguments; the LLM cannot supply them.

The serialized mode handler owns the final decision and invocation. Before
invocation it creates and syncs `shutdown-attempt.jsonl` in the mode working
directory, including the configured command, assessment ID, anomaly ID, evidence revisions and both
simulation results. It appends `accepted` or `failed_or_unknown` if it remains
running long enough. `accepted` means the command exited successfully, not that
power-off or cooling was observed. Invocation has a five-second timeout.

An existing journal suppresses further attempts, including after mode restart
or reboot; keep `persist_work_dir: true`. After investigating an unsuccessful or
interrupted attempt, archive/remove the journal while the mode is stopped to
permit a fresh assessment and attempt. Reconfiguration does not clear the
in-process attempt latch. This deliberately avoids automatically retrying an
action whose host outcome may be unknown.

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
| `planner.max_turns` | `6` |
| `planner.total_timeout_ms` | `120000` |
| `planner.provider_attempts` | `1` |
| `planner.provider_retry_backoff_ms` | `250` |
| `planner.repair_attempts` | `3` |
| `planner.limits.max_prompt_chars` | `1600` |
| `planner.limits.max_response_chars` | `800` |
| `planner.limits.response_size_multiplier` | `8` |
| `planner.limits.tool_result_max_chars` | `2000` |
| `planner.limits.context_tool_output_tokens` | `128` |
| `planner.limits.assessment_rationale_max_chars` | `400` |
| `planner.limits.assessment_uncertainty_max_chars` | `160` |
| `planner.limits.forecast_risk_max_items` | `2` |
| `planner.limits.forecast_risk_max_chars` | `100` |
| `planner.limits.selection_reason_max_chars` | `200` |
| `planner.limits.textual_assessment_rationale_max_chars` | `200` |
| `planner.limits.textual_assessment_uncertainty_max_chars` | `100` |
| `planner.limits.textual_selection_reason_max_chars` | `120` |
| `evidence.max_items` | `16` |
| `evidence.history_samples_per_source` | `8` |
| `evidence.require_telemetry` | `true` |
| `evidence.require_board` | `true` |
| `replanning.require_board_snapshot` | `false` |
| `replanning.replan_on_board_change` | `true` |
| `replanning.failed_plan_retry_ms` | `0` (disabled) |
| `observability.decision_trace` | `false` |
| `observability.trace_max_chars` | `1000` |
| `observability.candidate_value_max_chars` | `120` |
| `observability.diagnostic_max_chars` | `240` |
| `observability.simulation_trace_max_files` | `16` |
| `observability.simulation_trace_max_fields` | `32` |

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
its one available operation and embeds that operation's compact JSON Schema in
the prompt. Textual assessment prompts require all mandatory fields first, use
shorter rationale and uncertainty limits, and omit optional forecast risks so a
complete result fits small output budgets. The mode then feeds the parsed result
through the same host-owned assessment, selection, simulation, staleness, and
board-conflict validation as a native tool call. Malformed, truncated,
ambiguous, or oversized textual responses fail closed.

The `safe-llm-adapter` crate exposes `LlmAdapter`, `LlmAdapterFactory`, and
`AdapterRegistry` for mission-specific Rust adapters. Custom adapters must be
linked into the mode binary and registered at startup; runtime shared-library
loading is not supported.

All mission-facing wording can be changed without recompiling under `prompts`:

```json
{
  "prompts": {
    "planner_instructions": "Build a mission-specific evidence-backed assessment.",
    "assessment_instructions": "Treat persistent battery temperature excursions as thermal candidates.",
    "selection_instructions": "Prefer the least disruptive eligible recovery.",
    "multiple_calls_repair": "Use exactly the one operation currently offered.",
    "selection_repair": "Retry with exact configured identifiers.",
    "assessment_tool_description": "Complete this mission's thermal assessment.",
    "telemetry_tool_description": "Read the newest SAFE telemetry evidence.",
    "board_tool_description": "Read current command intent.",
    "selection_tool_description": "Select one eligible recovery.",
    "textual_transport_instructions": "Return only one compact JSON object.",
    "textual_assessment_guide": "Emit all required assessment keys using exact IDs.",
    "textual_selection_guide": "Emit all required selection keys using exact IDs.",
    "textual_generic_guide": "Emit required keys before optional keys."
  }
}
```

The mode appends evidence, candidates, action IDs, JSON schemas, and fail-closed
identifier rules in host code. Configurable prose cannot replace those safety
contracts. `schema_version` is required and currently must be `1`; removed flat
fields such as `goal`, `max_decision_attempts`, and `decision_trace` are rejected.

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

For a terminal demo, set `observability.decision_trace` to `true` in the mode's
`mode_config`. The advisor emits a compact, ordered `LLM DEMO` trace for the
configured candidates, each adapter request, the model's selected action and
rationale, validation or repair attempts, and the command-board proposal. The
trace is an auditable decision summary, not hidden model chain-of-thought.

This `mode_config` is a compact local-demo example:

```json
{
  "llm": {
    "adapter": {
      "kind": "ollama",
      "config": {"endpoint": "http://127.0.0.1:11434/api/generate"}
    },
    "model": "mistral:7b"
  },
  "schema_version": 1,
  "observability": {"decision_trace": true},
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
| One or more actionable candidates | Ask the configured adapter to assess evidence before any recovery selection. |

The same candidate set is not planned repeatedly until its signature changes.
When `replanning.require_board_snapshot` is true, planning waits for the first board
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

`simulation` is optional for assessment-only operation; eligible shutdown rules
require initialized simulation contracts at configuration validation. It names
one trusted local `eds_path` and allow-listed
scenarios. A scenario declares applicable nominal-rule IDs, allowed actions,
duration, trusted constant or telemetry-derived patch bindings, optional bounded
parameters, and compact numeric output metrics. The model never receives EDS
paths, patches, raw frames, stdout, stderr, shell arguments, or filesystem paths.
Post-selection validation creates independent baseline and recovery
`SedaroSimulator` runs. `simulation.max_runs` must allow at least those two runs.
There is no cloud API.

For executable recovery scenarios, configure `simulation.initialization`:

- `source`: exact telemetry source; `epoch_path`: dot-separated payload path to
  finite UTC MJD, passed to EDS as `--start`.
- `patches`: shared typed state patches with `agent_id`, `engine`, `field`, `type`
  and exactly one of `value`, `telemetry_path`, or `telemetry_paths` (component
  paths for a vector). Supported types are `f64`, `bool`, `#[f64; 3]`,
  `#[f64; 4]`, and constant empty lists for clearing bundled schedules. Optional
  `scale` applies to numeric values/components, allowing explicit unit conversion.
- `requirements`: input paths with either an `expected` non-null value or
  inclusive numeric `min`/`max` bounds.
- `command_schedules`: bindings with `id`, `agent_id`, `engines`, `field`, and
  `action_modes` mapping configured pointing actions to model-specific mode IDs.
  The runner clears these schedules for the baseline and writes the selected
  recovery's mode at the common start epoch as `[(f64, str)]`.
- `compute_power_bindings`: constant-watt operating/shutdown load bindings for
  mode-local shutdown, as described above.

Both scenarios share the initialization, state descriptors and horizon. State
descriptor paths and sources are now checked at runtime; shared patch targets
cannot be overwritten by per-scenario patches. The action's command-schedule or
compute-power label must
resolve to an executable binding. Numeric-only legacy baseline scenarios remain
supported, but label-only recovery scenarios without initialization fail before
their EDS run. Existing generic examples need deployment-specific initialization
before they can issue recovery commands.

This runner models a standalone recovery with an empty command board. It rejects
existing proposals, approvals and source-of-truth commands because they are not
yet projected into the baseline/recovery schedules. See the
[synthetic pointing profile](./testdata/pointing_profile.json) for the state and
schedule binding structure. Executable model IDs and fields belong in deployment
configuration.

`simulation.viability` configures the IDs used for final SOC, minimum SOC,
maximum SOC degradation, and the quantity name that identifies temperature
metrics. Its defaults are `final_state_of_charge`, `minimum_state_of_charge`,
`maximum_state_of_charge_degradation`, and `temperature`.

The adapter returns only normalized completion text and finish status. The mode
then enforces the response size, strict JSON parsing, selected anomaly ID,
eligible action, and exact evidence path. HTTP errors, timeouts, malformed
responses, token-limit truncation, empty responses, oversized responses, and
provider failures are retried up to `planner.provider_attempts`; malformed or
invalid selections may consume up to `planner.repair_attempts`. Exhausting
either budget fails closed.

The telemetry and board tools are read-only. Their responses are bounded to
`planner.limits.tool_result_max_chars` characters and report `unavailable` until SAFE has broadcast the
corresponding snapshot. They read the newest snapshot available at invocation
time, but do not change the frozen candidate/action allow-list or bypass SAFE
board and gatekeeper validation.

## SAFE Integration

For board-backed recovery actions, the advisor emits:

```text
TimedCommand::Now(Command::<configured action>)
```

to SAFE as a board proposal. Emitting a command means submitting it to SAFE;
it does not by itself mean that a host vehicle executed it. Gatekeeper and
platform egress behavior is described in
[`../safe/docs/runtime-operations.md`](../safe/docs/runtime-operations.md).
The local `shutdown` action instead executes inside the mode after simulation
and final validation, without a SAFE command enum or egress round trip.

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
      "schema_version": 1,
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
- [`testdata/pointing_profile.json`](./testdata/pointing_profile.json): synthetic
  baseline, sun-yaw and nadir state/schedule bindings for deterministic tests.
- [`testdata/shutdown_profile.json`](./testdata/shutdown_profile.json): synthetic
  compute-on/shutdown power bindings. Shutdown tests use fake executors.

Run the advisor unit and integration tests with:

```bash
cargo test -p mode-anomaly-recovery
```

The ignored, LLM-free EDS pointing test takes its deployment configuration and
telemetry from files rather than a checked-in mission configuration:

```bash
ANOMALY_RECOVERY_EDS_CONFIG=/path/to/mode-config.json \
ANOMALY_RECOVERY_EDS_TELEMETRY=/path/to/telemetry-payload.json \
cargo test -p mode-anomaly-recovery \
  local_eds_executes_distinct_baseline_sun_and_nadir_scenarios -- --ignored --nocapture
```

Supply the inner `mode_config` object and decoded telemetry payload, with
scenarios ordered baseline, sun-yaw recovery, nadir recovery. The test expects
compatible CDH/GNC pointing outputs and fraction-valued SOC metrics. Set
`ANOMALY_RECOVERY_EDS_PATH` to optionally override the configured EDS path.

The opt-in live pointing example reads an outer mode configuration from
`SAFE_LIVE_POINTING_CONFIG`, sends fake high-temperature telemetry through
the real SAFE mode transport, and runs the configured real EDS twice for the
baseline/recovery viability check. It requires a configured provider, its API
credentials, and an executable pointing EDS configuration. It rejects any action
catalog containing shutdown before launching the real mode. The checked-in
shutdown example is not suitable for this pointing-only live test:

```bash
SAFE_LIVE_POINTING_CONFIG=/path/to/pointing-autonomy-config.json \
cargo test -p mode-anomaly-recovery --features https \
  --test live_openai_simulation_e2e \
  -- --ignored --nocapture
```

The test captures mode logs and verifies thermal assessment, post-selection
simulation, power-only thermal separation, and command proposal stages. It is
ignored by default because it consumes OpenAI API and EDS resources.
