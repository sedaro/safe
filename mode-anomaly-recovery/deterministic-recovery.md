# Deterministic anomaly recovery

The anomaly AutonomyMode owns the complete recovery procedure: threshold checks,
shutdown-attempt persistence, the reboot-surviving wait, recovery checks, and a
normal `TimedCommand::NOOP` to finish its turn. It uses SAFE's existing protocol
and activation configuration.

## Behavior

1. On activation, load `recovery-state.json` from the mode working directory.
2. If there is an unfinished episode, evaluate its original deadline before any
   new shutdown trigger. Hold for at least `minimum_hold_secs` (default 6000).
3. With no unfinished episode, a fresh critical power **or** temperature
   measurement, confirmed by `trigger_samples`, starts recovery. Durably record
   the attempt and deadline before invoking `shutdown_command`.
4. After SCP restarts the computer, the same mode restores the episode and waits.
   `on_tick` checks time without blocking telemetry callbacks or heartbeats.
5. At/after the deadline, require fresh valid power **and all configured thermal
   channels** to meet their recovery thresholds, sample counts, and measured dwell.
   Otherwise continue waiting without another automatic shutdown.
6. Persist completion and emit one `NOOP` per activation. Normal telemetry
   reception and SAFE's configured selection rules advance to the next mode.

The thresholds are inclusive: power `<= trigger`, temperature `>= trigger`;
recovery requires power `>= recover`, temperature `<= recover`. Configure
hysteresis (`power.recover > power.trigger`, thermal `recover < trigger`).

Inactive mode instances may collect telemetry but never invoke shutdown or emit
`NOOP`. After yielding, the mode waits for its next activation before taking a
new recovery action. A new episode can start after an earlier one completes.

## Mode configuration

Use [`testdata/deterministic_recovery_profile.json`](testdata/deterministic_recovery_profile.json)
as the `mode_config` object of your anomaly entry. It is a **synthetic, mock-shutdown
fixture**: `/bin/true` allows bench testing without stopping the computer. Replace
its power/temperature bindings and thresholds with deployment values. Set
`shutdown_command` to the host command (for example `["/sbin/shutdown", "-h", "now"]`)
only for the real SCP restart test/deployment.

Setting `mode_config.recovery` selects this procedure. Omitting it retains the
existing LLM assessment/simulation workflow. The deterministic procedure requires
neither an LLM configuration nor an EDS configuration. Its `action_catalog` is
empty, and it does not accept simulation configuration.

Each measurement declares:

- `id`, exact telemetry `source`, and dot-separated payload `path`;
- `units` and optional positive `scale` (default 1);
- `timestamp_path`: **that sensor's UTC measurement time**, in Unix seconds,
  with optional `timestamp_scale` (e.g. 0.001 for milliseconds);
- optional `validity_path` and exact JSON `valid_value` (e.g. `0` for a result code);
- `trigger` and `recover`, expressed in the declared, scaled units.

Duplicate or out-of-order measurement timestamps do not increase persistence
counts. Invalid readings and excessive measurement gaps reset confirmation.
Repackaging an old sensor value into a new telemetry frame does not make it fresh.
All recovery evidence must be fresh at release, and measured timestamps—not just
elapsed wall time—must establish `recovery_dwell_secs`.

`trusted_system_utc: true` declares that the deployment supplies synchronized,
reboot-surviving system UTC before SAFE starts. Monotonic elapsed time detects
in-process UTC jumps beyond `max_clock_step_secs`; detected uncertainty keeps the
mode waiting until it is restarted with a corrected clock. `TelemetryFrame.ts_mono`
is not used as a cross-reboot clock. New mode processes require new sensor
measurements after startup before yielding.

## Existing activation-rule handoff

[`recovery-routing.example.json`](recovery-routing.example.json) contains routing
fields to **merge into** the corresponding entries in your deployment's autonomy
configuration. Keep each entry's executable, resource limits, and mode-specific
configuration. Enable and configure the downstream modes you intend to run.
The example is routing metadata, not a complete launch configuration.

It implements this cycle using the existing `Hysteretic` rules:

```text
AnomalyRecovery → MissionPlanning → CoorbitalEvasion → AnomalyRecovery
```

- Anomaly is the always-eligible fallback at priority 0. The other two modes have
  priority 1 and are eligible only after their predecessor has produced output.
- Each active mode stays selected until `LastPlannedAutonomyModeRef` equals its
  own UUID. Anomaly's only board command in this procedure is its final `NOOP`.
- On a fresh start with no previous command, the downstream entry conditions
  are unresolved, so anomaly is selected. Its unresolved exit condition retains it.
- During recovery there is no `NOOP`, so anomaly stays selected, including after
  restoration of SAFE's flight state. Its lower fallback priority does not bypass
  the existing hysteresis hold.
- On `NOOP`, SAFE records anomaly as the last-planned mode. At the next normal
  activation reevaluation (e.g. the next telemetry frame), the next eligible mode
  takes over. The anomaly mode itself names no successor.

The UUID literals are derived from the exact names in the example. If names
change, update those literals using the IDs reported by `safectl get modes`.
Manual activation overrides retain SAFE's existing pinning behavior; use the
activation rules for this automatic sequence.

The example uses SAFE's existing meaning of “last planned”: **any command output**,
including `NOOP`, updates it. Downstream modes need to emit a command or `NOOP` to
finish their turn. Mode selection controls who plans next; already-delivered
spacecraft schedules retain the platform's existing behavior.

## Persistence and diagnostics

Set `persist_work_dir: true`, keep the mode name stable, and place SAFE's runtime
state on reboot-surviving storage. A persistent working-directory flag cannot
make a tmpfs survive a computer restart.

The mode writes:

| File | Purpose |
| --- | --- |
| `recovery-state.json` | Episode ID, trigger measurements/reasons, policy, attempt time, original deadline, invocation outcome, completion time. |
| `recovery-status.json` | Active/waiting state, latest measurements, remaining time, reason for waiting, clock and advisor status, whether `NOOP` was sent. |
| `advisory-assessment.json` | Latest optional assessment and associated episode ID. |

State changes use atomic replacement, file synchronization, and directory
synchronization. New episode IDs use the shutdown-attempt timestamp (for example,
`shutdown-1800000001`); previously persisted UUID-string IDs also remain readable.
A failed or uncertain shutdown invocation counts as the episode's
one attempt. A persistence failure prevents shutdown and keeps the mode waiting.
Configuration changes cannot shorten an unfinished episode: restore the recorded
policy to continue it. Corrupt state is reported as a waiting reason.

An older `shutdown-attempt.jsonl` has no trustworthy deadline. If one exists
without a new recovery record, the mode waits and reports that explicit migration
is required. Archive it only after determining the prior attempt's outcome and
choosing the desired deployment reset; the mode does not invent a timestamp.

## Optional LLM advisor

Set `advisory.enabled: true` and supply the usual `llm` object to enable bounded,
assessment-only inference. The advisor has no command or recovery-control handle.

- Existing `nominal_profiles` provide noncritical investigation triggers; their
  `eligible_actions` are empty in this configuration.
- After recovery, an assessment can summarize trigger evidence, recent readings,
  board intent, likely contributors, evidence gaps, and operational recommendations.
- Inference runs asynchronously after the deterministic check/handoff. Provider
  failures never delay `NOOP` or decide shutdown/release.
- While recovery is waiting, pending inference is cancelled and new requests are
  suppressed, including after the minimum 100 minutes if recovery is incomplete.
- `min_interval_secs` (default 300) and `history_samples` (default 64, max 256)
  bound assessment frequency and evidence history. Responses are validated and
  written as advisory records, not executed as commands.

For a local model server, configure `pause_command` and `resume_command` to manage
that service; merely cancelling the client request may leave server-side inference
running. These are executable/argument arrays, not shell strings. Set
`local_inference: false` for a remote provider. Local service failures are reported
in the status file and retried at a bounded cadence. A critical shutdown is not
delayed waiting for an advisory service command.

## Verification and FlatSat sequence

```bash
cargo test -p mode-anomaly-recovery
cargo test -p mode-anomaly-recovery --test deterministic_recovery_integration
cargo build --workspace
cargo test --workspace
```

The transport tests launch the actual mode executable using only protocol v2.
They exercise inactive behavior, one `NOOP` per activation, the unchanged SAFE
selector with the example rules, restart with the original deadline, persistent
anomalies, timer-driven release, and LLM failure isolation. Controller tests use
injected timestamps to cover the complete 6000-second hold without a real wait.

For FlatSat, first use `/bin/true` and an accelerated hold, then test actual
shutdown/SCP restart. Finally restore `minimum_hold_secs: 6000`, measure idle
power/thermal response, and exercise both continued waiting and successful
handoff. The software tests do not exercise physical SCP restart or establish
deployment-specific power/thermal thresholds.
