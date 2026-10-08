# Deterministic anomaly recovery

The anomaly AutonomyMode confirms critical sensor measurements and owns durable
SOC/sun-point and thermal/shutdown episodes. It uses SAFE's existing command-board
protocol and activation configuration; no shared handoff-policy change is needed.

## Behavior

On activation, load `recovery-state.json`. Resume an unfinished episode before
considering a new trigger. Fresh critical measurements confirmed by
`trigger_samples` start a new episode only while the mode is active.

### Low SOC

1. Durably record the confirmed trigger and the original sun-point deadline:
   confirmation time + `sun_point_delay_secs` (default **600 seconds**).
2. Wait for a known command-board snapshot. Cancel every non-rejected proposal,
   including pending/approved commands, `NOOP`s, and commands from other modes.
   Audit history remains on the board.
3. Submit one `TimedCommand::Scheduled { cmd: PointSunYaw, gps_time }`, using
   `safe-time` to convert the original UTC deadline to GPS seconds. Cancellation
   outputs precede this proposal. It follows the normal gatekeeper/publication path.
4. Reconcile board snapshots to track proposed, approved, rejected, or missing
   sun-point commands. While active, cancel newly arriving commands except the
   episode's own sun-point command. Repeated samples never reset the deadline or
   repeat the proposal.
5. The next mode's activation configuration requires last-planned AnomalyRecovery
   **and `augmented.state_of_charge > recovery threshold`**. Low SOC therefore
   keeps the always-eligible anomaly fallback selected. The timed command itself
   supplies the last-planned condition; SOC recovery needs no `NOOP`.
6. A fresh valid SOC value strictly above `power.recover` records episode completion,
   even if SAFE has already deactivated the mode before forwarding that telemetry.
   SOC completion has no thermal-channel, recovery-sample, dwell, or time gate.

**Ten minutes is the command delay, not a minimum active-mode duration.** If SOC
recovers earlier, the next mode may plan immediately while sun-pointing remains
scheduled. This mode does not cancel that schedule on handoff; subsequent normal
planning and the platform still control whether it is superseded or executed.
Board approval/publication is intent, not spacecraft attitude acknowledgement.
Command rejection/uncertainty is reported, but cannot override the SOC-only
activation gate. A later independent low-SOC episode can start after recovery.

SOC wins if SOC and thermal triggers are confirmed together. A new thermal trigger
does not interrupt an unfinished SOC episode. Thermal conditions are evaluated
again on a later active turn after the SOC episode completes.

### Thermal-only recovery

1. Durably record one shutdown attempt and a deadline before invoking
   `shutdown_command`.
2. After SCP restarts the computer, restore the original deadline and hold for at
   least `minimum_hold_secs` (default **6000 seconds / 100 minutes**).
3. At/after the deadline, require fresh valid power **and all configured thermal
   channels** to satisfy recovery thresholds, sample counts, and measured dwell.
   Otherwise continue waiting without another automatic shutdown.
4. Persist completion and emit one `NOOP` per activation for the existing handoff.
   A nominal turn with no critical episode also emits one `NOOP`.

The thresholds are inclusive: power `<= trigger`, temperature `>= trigger`;
thermal-episode recovery requires power `>= recover`, temperature `<= recover`.
SOC completion and the example activation gate use power **`> recover`**. Configure
hysteresis (`power.recover > power.trigger`, thermal `recover < trigger`).

Inactive instances may collect telemetry, observe board progress, and persist SOC
completion, but never cancel commands, invoke shutdown, or emit commands. After
handoff to another mode, recovery waits for its next activation before acting. If
it remains selected as the fallback after a nominal `NOOP`, a new confirmed SOC
trigger can still start recovery without a deactivate/activate round trip.

## Mode configuration

Use [`testdata/deterministic_recovery_profile.json`](testdata/deterministic_recovery_profile.json)
as the `mode_config` object of your anomaly entry. It is a **synthetic fixture**:
`/bin/true` allows thermal bench testing without stopping the computer. Its SOC
trigger/recovery thresholds are 0.15/0.3 (fractions), with a 600-second sun-point
delay. Replace its sensor bindings and thresholds with deployment values. Set
`shutdown_command` to the host command (for example `["/sbin/shutdown", "-h", "now"]`)
only for the real SCP restart test/deployment.

Setting `mode_config.recovery` selects this procedure. Omitting it retains the
existing LLM assessment/simulation workflow. The deterministic procedure requires
neither an LLM configuration nor an EDS configuration. Its `action_catalog` is
empty. Optional `simulation` configuration supplies counterfactual evidence to
the LLM advisor when `advisory.enabled` is true; it never gates critical recovery.

For a complete synthetic example with paired power simulations and LLM analysis,
see [`testdata/recovery_advisory_profile.json`](testdata/recovery_advisory_profile.json).
Replace its example telemetry bindings and model settings for your deployment.

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
All controller recovery evidence must be fresh at completion. For thermal episodes,
measured timestamps—not just elapsed wall time—must establish `recovery_dwell_secs`.
These thermal recovery persistence settings do not gate SOC activation handoff.

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

It implements this cycle using existing `Hysteretic` and `Immediate` rules:

```text
AnomalyRecovery → MissionPlanning → CoorbitalEvasion → AnomalyRecovery
```

- Anomaly is the always-eligible fallback at priority 0. The other two modes have
  priority 1 and are eligible only after their predecessor has produced output.
- Anomaly and CoorbitalEvasion use hysteretic holds until the last-planned UUID
  matches their own. MissionPlanning uses an immediate condition requiring both
  last-planned AnomalyRecovery and `augmented.state_of_charge > 0.3`.
- On a fresh start with no previous command, the downstream entry conditions
  are unresolved, so anomaly is selected. Its unresolved exit condition retains it.
- A SOC sun-point proposal updates last-planned immediately. Below or exactly at
  the SOC recovery threshold, MissionPlanning remains ineligible and anomaly is
  reselected as the fallback. Above it, MissionPlanning can take over on the next
  normal telemetry reevaluation, before the sun-point time if SOC recovers early.
- A thermal episode emits no command until its final `NOOP`, preserving the
  original hold. `NOOP` then makes MissionPlanning eligible if its SOC gate passes.
- The anomaly mode itself names no successor.

The routing threshold **must match `recovery.power.recover` in scaled units**.
`augmented.state_of_charge` is a deployment telemetry payload path, not a mode
status field. The synthetic fixture binds the same value through `battery.soc`;
test telemetry provides both. Supply that augmented field in each relevant latest
frame, or change the routing path to your actual SOC path. The selector does not
check sensor validity, freshness, confirmation counts, or dwell: its raw latest-frame
SOC condition is the authority for handoff. A missing field makes MissionPlanning
ineligible, so anomaly remains the fallback. Ensure no other downstream rule
bypasses the intended SOC gate. Existing manual overrides and hysteretic holds
in other modes retain their normal behavior.

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
| `recovery-state.json` | Versioned episode kind, trigger evidence/policy, original deadline, thermal invocation outcome or SOC cancellation targets/confirmations and sun-point submission/board status, completion time. |
| `recovery-status.json` | Active/waiting state, latest measurements, remaining time, reason for waiting, clock/advisor status, `noop_sent` and `soc_handoff_ready`. For SOC, remaining time is until the scheduled command, not a handoff hold. |
| `advisory-assessment.json` | Latest optional report: assessment or provider error, simulation runs/comparisons, frozen telemetry revision, and associated episode ID. |

State changes use atomic replacement, file synchronization, and directory
synchronization. New episode IDs use the trigger timestamp (`soc-1800000001` or
`shutdown-1800000001`); previously persisted UUID-string IDs also remain readable.
A failed or uncertain shutdown invocation counts as the episode's
one attempt. A persistence failure prevents shutdown and keeps the mode waiting.
Configuration changes cannot shorten an unfinished episode: restore the recorded
policy to continue it. Corrupt state is reported as a waiting reason.

New records use schema version 2. Version-1 shutdown records, including unfinished
records originally triggered by low SOC, retain their original shutdown/cooldown
procedure and deadline; they are not silently converted to sun-point episodes.
Complete the old episode under its recorded policy, or explicitly archive/reset
its state after determining the prior shutdown outcome before adopting SOC recovery.

Sun-point submission intent is persisted **before** emitting the command. On
restart, a matching own-mode sun-point proposal at the original GPS time is adopted
without resubmission, even if it was rejected. If a crash occurred between durable
reservation and board confirmation and no matching proposal is present, status
reports `submission reserved; awaiting board confirmation`; it does not invent a
new deadline or risk a duplicate command. Resolve an uncertain attempt explicitly
before resetting its state. Cancellation targets and confirmations are persisted;
unconfirmed cancellations can be repeated on restart and are idempotent board effects.

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
- If `simulation` is configured, applicable baseline/recovery pairs run first and
  their metrics, constraint outcomes, and errors are provided to the LLM. Missing
  simulation inputs or failed runs are evidence gaps; analysis can still proceed.
- Inference runs asynchronously after the deterministic check/handoff. Provider
  failures never delay deterministic actions or decide shutdown/release.
- While recovery is waiting, pending inference and EDS work are cancelled and new
  requests are suppressed, including after the minimum 100 minutes if recovery is
  incomplete. EDS runs use the existing subprocess timeout and kill-on-drop behavior.
- `min_interval_secs` (default 300) and `history_samples` (default 64, max 256)
  bound assessment frequency and evidence history. Responses are validated and
  written as advisory records, not executed as commands.
- `advisory.instructions` supplies mission-specific assessment context. Older
  multi-turn planner prompts are not used by this assessment-only path. The prompt
  builder removes oldest history with an omission count to fit its estimated
  context budget, while retaining current measurements and simulation evidence.

Model service management is optional. Leave both `pause_command` and
`resume_command` empty to keep the server running while cancelling/suppressing
this mode's inference requests. Client cancellation may leave an already-submitted
server-side inference running. To manage a local service as well, configure both
commands as executable/argument arrays. `local_inference` identifies a local or
remote provider. Service failures are reported in the status file and retried at
a bounded cadence. A critical shutdown is not delayed by a service command.

### Advisory simulation applicability

Use the existing `simulation.initialization` and `simulation.scenarios` schema.
Before a noncritical assessment, run pairs whose recovery scenario lists a current
candidate in `applicable_rule_ids`. Post-recovery assessments may run configured
pairs from the **current recovered snapshot**, rather than reconstructing the
pre-shutdown orbit. Respect `max_runs` across the assessment and `run_timeout_ms`
per scenario. Each pair shares the frozen snapshot, horizon, and evidence revision.

The existing SOC/units/constraint checks evaluate each pair. Both successful and
failed comparisons are recorded; failed constraints do not erase collected metrics.
`allowed_actions` and `modeled_action` describe simulated counterfactuals in this
path. They grant no execution authority; `action_catalog` and rule-level
`eligible_actions` remain empty.

The standalone scenarios currently omit existing board plans. They therefore run
only with a known board containing no outstanding effectful commands. `NOOP` and
rejected proposals do not block them. Missing/effectful board context produces an
explicit skipped-simulation reason and allows LLM analysis to proceed. A relevant
board change cancels in-flight advisory work. Simulation failure or LLM failure
does not affect shutdown, the cooldown deadline, or release.

## Verification and FlatSat sequence

```bash
cargo test -p mode-anomaly-recovery
cargo test -p mode-anomaly-recovery --test deterministic_recovery_integration
cargo build --workspace
cargo test --workspace
```

The transport tests launch the actual mode executable using only protocol v2.
They exercise cross-mode board cancellation, a single timed sun-point proposal,
SOC/thermal overlap, inactive completion and early SOC handoff, rejection/uncertainty,
restart reconciliation, the unchanged selector with the SOC-gated example rules,
thermal timer-driven release, and LLM failure isolation. Controller tests use
injected timestamps to cover SOC recovery and the complete thermal 6000-second hold.

For FlatSat, first use `/bin/true` and an accelerated hold, then test actual
shutdown/SCP restart. Finally restore `minimum_hold_secs: 6000`, measure idle
power/thermal response, and exercise both continued waiting and successful
handoff. The software tests do not exercise physical SCP restart or establish
deployment-specific power/thermal thresholds.
