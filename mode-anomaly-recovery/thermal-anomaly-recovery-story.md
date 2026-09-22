# Thermal Anomaly Assessment and Recovery

See the [implementation plan](./thermal-anomaly-recovery-plan.md) for delivery
milestones, dependencies, and verification criteria.

## User Story

As a mission operator, I want the anomaly-recovery mode to use an LLM with
telemetry-fetching, command-board inspection, and simulation tools to assess
the spacecraft's thermal state, so that it can judge whether a thermal anomaly
is occurring, explain the evidence and uncertainty, and propose an appropriate
recovery action when warranted.

The primary output is an evidence-backed thermal assessment. Recovery selection
follows that assessment. A configured rule violation starts an investigation;
the assessment determines whether the observations support a thermal anomaly,
an expected operational transient, or an inconclusive result. Configured limits
remain explicit in the assessment even when a transient has an explanation.

This document defines target behavior. The implementation gaps below describe
the `llm-final` branch, including the staged telemetry/board tools and adapter
changes present during this review.

## Assessment Outcomes

Each completed investigation records one of these outcomes:

| Outcome | Meaning | Recovery disposition |
| --- | --- | --- |
| `thermal_anomaly` | Available evidence supports an ongoing thermal anomaly. | Propose an eligible, justified recovery action, or explain why monitoring or operator review is needed. |
| `no_thermal_anomaly` | Sufficient current evidence supports nominal behavior or an expected transient within the mission's allowed operating envelope. | Continue monitoring without proposing recovery. |
| `inconclusive` | Missing, stale, conflicting, or inadequate evidence prevents a defensible assessment. | Identify the missing evidence and request reassessment or operator review without forcing an action. |

A predicted future limit breach is recorded as a forecast risk, separately from
the assessment of whether an anomaly is occurring now. An LLM's stated
confidence is an explanation of evidential strength, not a calibrated probability.

## Investigation Workflow

1. **Open an assessment episode.** Configured thermal rules and persistence
   criteria identify a suspected condition. Capture the triggering source,
   timestamps, values, limits, and profile/rule IDs. An investigation is useful
   even when no recovery action is configured or only one is eligible.
2. **Fetch thermal context.** Use `get_latest_telemetry` to inspect relevant
   temperatures and available supporting state, such as attitude, heater state,
   power load, and eclipse/illumination. Include a bounded recent history or
   derived trend so persistence and rate of change can be assessed. Report
   source, units, timestamps, freshness, and missing or invalid measurements.
3. **Inspect the command board.** Use `get_command_board_state` to identify
   commands that could explain thermal behavior or affect recovery. Relate
   proposal IDs, timing, approvals, rejections, and source-of-truth membership
   to the thermal episode. Board state establishes command intent; execution
   must be corroborated by execution feedback or telemetry when available.
4. **Evaluate explanations with simulation.** Use `run_eds_simulation` for
   applicable, configured scenarios. Compare observed thermal behavior with a
   baseline that represents the current state and relevant command schedule.
   When considering recovery, compare that baseline with applicable recovery
   scenarios under comparable initial conditions and time horizons. Request
   additional runs only when they can resolve uncertainty and fit the budget.
5. **Synthesize the evidence.** Retain telemetry, board findings, and simulation
   results across tool calls. Explain whether the observed temperatures and
   trends fit expected behavior, indicate abnormal heating/cooling, or could
   reflect a measurement problem. Cite supporting and conflicting evidence,
   model assumptions, and gaps; distinguish observations from predictions.
6. **Record the assessment and disposition.** Produce one of the outcomes above
   with a concise rationale, severity, uncertainty, and evidence references.
   When recovery is justified, explain the expected thermal benefit, action
   preconditions, and interactions with existing commands before submitting a
   configured action as a SAFE board proposal.
7. **Reassess as the state changes.** Material telemetry or board changes
   invalidate affected conclusions and simulation inputs. Observe the response
   to any recovery command through subsequent telemetry. Close or reopen the
   episode using configured persistence/recovery criteria and avoid duplicate
   proposals for an already-addressed condition.

## Tool and Evidence Requirements

| Tool | Evidence needed by the LLM |
| --- | --- |
| `get_latest_telemetry` | Relevant source-scoped measurements, timestamps/version, units and freshness, plus bounded history/trends or an explicit indication that history is unavailable. |
| `get_command_board_state` | Thermally relevant command details, IDs, timing, status and source-of-truth membership, with snapshot version/freshness and explicit execution uncertainty. |
| `run_eds_simulation` | Scenario/run ID, success or failure, input telemetry/board versions, modeled command assumptions, horizon, and thermal metrics with units. Useful metrics include peak/final temperature, time outside limits, and temperature trend; power metrics provide supporting context. |
| Assessment completion (new capability) | Outcome, evidence references, uncertainty, and disposition, including a valid completion without a command. |
| `select_recovery_action` | A configured candidate/action pair linked to the completed assessment, action preconditions, and relevant successful simulation evidence when required. |

Tool results must preserve provenance in a bounded evidence record. Large
snapshots should return relevant summaries or explicit omissions. Unavailable,
oversized, failed, or stale results are evidence gaps, never nominal readings.
Tool fetching reads SAFE-provided state; it must not imply an independent vehicle
measurement or command execution acknowledgment.

Simulation is diagnostic only when it models the thermal quantities and
conditions under investigation. A successful power-only run cannot establish
thermal recovery. Scenario/action allow-lists constrain what can be proposed;
they do not demonstrate that an action was modeled or improves temperature.

## Acceptance Criteria

1. **Assessment precedes recovery.** Given a thermal trigger, the mode performs
   an evidence-based assessment even if there is only one eligible action or
   none. It can complete with any of the three outcomes without inventing an
   action. A rule violation alone does not force a recovery proposal.
2. **Telemetry and board are considered together.** The assessment cites both
   tool results, or explicitly identifies their unavailability and its effect
   on the conclusion. It accounts for relevant planned commands without
   treating a proposal or approval as proof of execution.
3. **Evidence survives tool calls.** After fetching telemetry, inspecting the
   board, and running simulation in any supported order, the final assessment
   can reference all relevant results and their versions. Tool budgets permit
   this complete sequence and a final assessment.
4. **Simulation tests the thermal hypothesis.** When applicable thermal
   scenarios are configured, the assessment uses successful thermal results
   tied to the investigated state. A recovery comparison identifies the modeled
   action and baseline. Failed runs and power-only metrics are disclosed as
   limitations and cannot satisfy a thermal-simulation requirement.
5. **Uncertainty is actionable.** Stale/missing telemetry, contradictory sensors,
   unavailable board context, or simulation disagreement result in a qualified
   conclusion with the missing evidence identified. Insufficient evidence is
   represented as `inconclusive`, not silently interpreted as normal or as a
   requirement to choose an action.
6. **Decisions use current evidence.** If relevant telemetry clears or materially
   changes, or the command schedule changes during analysis, the mode refreshes
   the assessment or invalidates the pending decision before proposal. Changes
   in values or board state can trigger reassessment even if rule IDs are the
   same. Repeated equivalent inputs do not cause duplicate proposals.
7. **Recovery has an explicit basis.** A proposal references the assessment and
   an eligible configured action, explains its expected benefit and relevant
   preconditions, and accounts for existing/conflicting recovery commands.
   SAFE board and gatekeeper validation remain the command submission path.
8. **The result is auditable.** A structured record includes episode ID,
   assessment outcome, observed values/limits, evidence versions and paths,
   board command IDs, simulation run IDs/status/metrics, concise rationale,
   uncertainty, and recovery disposition. Tool/model failure is recorded as an
   incomplete assessment rather than a successful diagnosis.

### Acceptance Scenarios

| Scenario | Expected result |
| --- | --- |
| Persistent high/rising temperature, corroborating telemetry, and a baseline simulation that does not explain the observations | `thermal_anomaly`; propose recovery only when the configured action and evidence justify it. |
| A thermal trigger is followed by an explainable transient, corroborated operating state, and return to the allowed envelope | `no_thermal_anomaly`; record the transient and continue monitoring. |
| Missing/stale measurements or conflicting sensors prevent diagnosis | `inconclusive`; identify the additional observations needed. |
| Simulation fails or returns only battery state of charge | Report the simulation limitation; do not claim simulated thermal improvement. |
| The board already contains an applicable recovery command | Assess its status and observed effect; avoid a duplicate proposal. |
| Temperature recovers or a relevant command changes during an LLM/simulation turn | Invalidate the obsolete decision and reassess before proposing recovery. |

## Current Branch: Capabilities and Gaps

The branch provides native tool calls through Ollama and OpenAI-compatible
adapters, bounded latest-telemetry and command-board readers, configured local
EDS scenarios, and validation of candidate/action selections.

The following changes are needed to fulfill this story:

| Area | Current behavior | Needed for this story |
| --- | --- | --- |
| Diagnostic result | `planner.rs` completes through `select_recovery_action`, which requires an action. | Add a structured assessment result and explicit no-command completion. |
| Assessment entry | `runtime.rs::plan_current_candidates` skips candidates without actions and immediately emits a sole eligible action. | Assess suspected thermal conditions independently of action count. |
| Evidence retention | `fresh_selection_messages` replaces history with the latest tool result and frozen planning context. | Retain a bounded cumulative evidence record across telemetry, board, and simulation calls. |
| Telemetry context | `LiveContext` retains one latest frame, without a history window or per-source collection. | Provide source-scoped thermal context, trends, and freshness checks. |
| Board analysis | The board tool is optional and returns a snapshot; its use and command-aware reasoning are not required. | Require board-context consideration and identify explanatory, duplicate, or conflicting commands. |
| Simulation loop | `phase_tools` stops advertising simulation after the first run; even a failed run counts toward the selection prerequisite. | Support bounded baseline/recovery comparisons and require successful relevant evidence where simulation is required. |
| Thermal fidelity | The EDS is intentionally power-only and returns battery state of charge, not thermal quantities. | Keep thermal assessment telemetry/LLM-based; use the EDS only for action-specific power viability. Add a separate thermal model only if simulation-backed thermal claims become a requirement. |
| Action context | The tool planner's `planning_context` omits action-catalog descriptions and preconditions. | Supply the action semantics and preconditions needed to justify recovery. |
| Reassessment | Deduplication uses source/rule IDs; changed values and later board snapshots do not trigger replanning by themselves. Clearing candidates does not cancel an in-flight plan. | Invalidate obsolete plans and reassess on material evidence changes, including recovery. |
| Verification | The ignored live OpenAI test checks for an eligible emitted command, without supplying a board snapshot or asserting the diagnostic evidence. | Exercise all outcomes, cumulative tool evidence, simulation relevance/failure, board interactions, and stale-plan invalidation with deterministic fixtures. |

Implementation references: [`src/planner.rs`](./src/planner.rs),
[`src/runtime.rs`](./src/runtime.rs), [`src/types.rs`](./src/types.rs),
[`../safe-llm-adapter/src/lib.rs`](../safe-llm-adapter/src/lib.rs),
[`../safe/autonomy_mode_config.json`](../safe/autonomy_mode_config.json), and
[`tests/live_openai_simulation_e2e.rs`](./tests/live_openai_simulation_e2e.rs).
