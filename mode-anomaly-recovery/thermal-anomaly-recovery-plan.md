# Thermal Anomaly Recovery Implementation Plan

Implement the [thermal assessment story](./thermal-anomaly-recovery-story.md)
as six ordered, reviewable milestones. The first usable milestone is an
assessment-only mode that combines telemetry and board evidence and can finish
without a command. Thermal simulation and conditional recovery build on that
foundation.

## Design Decisions

- Keep nominal profiles as investigation triggers. Run assessment for zero,
  one, or multiple eligible actions; remove automatic single-action emission.
- Separate `AssessmentOutcome` (`thermal_anomaly`, `no_thermal_anomaly`,
  `inconclusive`) from investigation status (`completed`, `failed`, `cancelled`,
  `superseded`) and recovery disposition. A provider failure is not a diagnosis.
- Add `complete_thermal_assessment` as a native tool. Keep
  `select_recovery_action` as a subsequent, optional step available only after
  a validated anomaly assessment requests recovery evaluation.
- Use a host-owned, bounded evidence ledger. Rebuild provider-neutral prompts
  from that ledger instead of discarding earlier tool results. This fits the
  existing adapter abstraction without depending on provider-specific history.
- Make a mode-local coordinator own episode state and proposal submission.
  Planner workers return structured results; they no longer emit commands
  directly. Serialize invalidation and proposal decisions in the coordinator.
- Publish structured assessment events through the existing mode logging path,
  with episode/evidence IDs. Use that as the initial operator-visible result;
  a new SAFE wire message is not needed for the first delivery.

## 1. Assessment Contract and Explicit Completion

**Files:** `src/types.rs`, `src/config.rs`, `src/planner.rs`, `src/runtime.rs`.

- Define an episode ID, assessment revision, outcome, evidence references,
  uncertainty/gaps, forecast risks, and disposition (`monitor`, `operator_review`,
  `evaluate_recovery`). Allocate IDs and evidence metadata in host code.
- Add a strict `complete_thermal_assessment` schema. Validate outcome/disposition
  combinations, candidate IDs, cited evidence IDs, and bounded rationale. Keep
  forecast risk distinct from an ongoing anomaly.
- Allow an empty action catalog for assessment-only configuration, while still
  rejecting any rule that references an undefined action.
- Pass all triggered candidates into assessment, including those without actions.
  Remove the sole-action shortcut. Retain episode context after a trigger clears
  so a refreshed assessment can describe recovery or an explained transient.
- Introduce a structured planner result and assessment event instead of treating
  command output as the only successful completion. Update or remove obsolete
  action-only completion helpers/tests once their callers migrate.

**Exit checks:** deterministic adapter responses complete all three outcomes;
zero/one/many-action triggers enter assessment; invalid outcome/disposition or
invented evidence is rejected; assessment-only completion emits no command.

## 2. Cumulative Telemetry and Command-Board Evidence

**Files:** `src/types.rs`, `src/config.rs`, `src/planner.rs`; introduce
`src/evidence.rs` for collection, summarization, and evidence validation.

- Replace the single telemetry slot with bounded per-source history. Configure
  relevant field paths/units, history count/age, freshness thresholds, and
  material-change thresholds. Preserve source timestamps and track local receipt
  times separately; do not subtract timestamps from unrelated clock domains.
- Reject or flag out-of-order/duplicate samples for trend and persistence
  calculations. Compute rate of change only with a valid time basis; otherwise
  report that trend is unavailable.
- Summarize relevant measurements and board commands with stable evidence IDs,
  versions, timing, status, and explicit omissions. Include thermally relevant
  commands from other modes. Board approval/source-of-truth membership must not
  be represented as execution acknowledgment.
- Replace `fresh_selection_messages` with prompt construction from the cumulative
  evidence ledger, candidates, action descriptions/preconditions, and assumptions.
  Reserve prompt space for required evidence; fail explicitly when it cannot fit.
- Require telemetry and board tool attempts before assessment completion. Permit
  unavailable results, but require the assessment to acknowledge relevant gaps.
  Validate freshness and required-evidence policy in host code. Insufficient
  required evidence permits `inconclusive`, not an unsupported nominal result.
- Make turn, prompt, output, and total-time budgets consistent with the complete
  tool sequence. Reserve turns for final assessment and optional action selection;
  exhausted budgets produce a recorded incomplete investigation.

**Exit checks:** telemetry → board → telemetry retains both sources of evidence;
multiple sources do not overwrite one another; old samples do not establish
persistence; oversized snapshots disclose omissions; stale/missing evidence is
handled explicitly; board intent is distinguishable from observed execution.

## 3. Episode Lifecycle, Invalidation, and Reassessment

**Files:** `src/runtime.rs`, `src/types.rs`, `src/config.rs`; introduce
`src/coordinator.rs` for serialized episode transitions and planner results.

- Feed telemetry, board updates, activation/deactivation, configuration changes,
  and planner completions through a bounded mode-local coordinator. The shared
  `ModeHandler` currently has no tick/result callback: give the coordinator its
  own event/timer loop and output handle rather than waiting for another telemetry
  frame to process a worker completion.
- Track evidence versions separately from a material-state revision. New data
  updates the ledger; clearing a trigger, changing a relevant command, violating
  freshness, or crossing configured material thresholds supersedes pending work.
- Cancel superseded workers and reject late results by episode/revision at the
  serialized proposal-submission boundary. Update shared state before launching
  replacement work. Define current state as the latest SAFE input processed by
  the coordinator at submission; later inputs cause a new assessment.
- Add bounded replan debounce/retry and freshness timers. Repeated equivalent
  inputs must not restart analysis continuously. Planner errors must not leave
  an episode permanently suppressed by `last_plan_signature`.
- Track suspected, assessing, monitoring/recovering, and closed episode states.
  Require configured recovery persistence to close; retain the closing assessment.
  Invalidate work immediately on deactivation/shutdown and reset it on reconfigure.

**Exit checks:** a blocked fake planner cannot submit after temperature clears,
a relevant board change, deactivation, or reconfiguration; changed temperatures
with identical rule IDs trigger reassessment; equivalent frames do not cause a
replan storm; failed planning retries within bounds; freshness expires without
new telemetry; unrelated-source frames do not clear a thermal episode.

**First usable delivery:** milestones 1–3 provide assessment-only thermal
investigation, audit events, and deterministic lifecycle coverage.

## 4. State-Aligned Thermal Simulation

**Files:** `src/config.rs`, `src/planner.rs`; introduce `src/simulation.rs` for
scenario execution/results; thermal fixtures under `testdata/`.

- Distinguish baseline and recovery scenarios. Give recovery scenarios an
  explicit modeled action and baseline association; separate that meaning from
  an action allow-list. Add metric quantity/units and scenario input requirements.
- Seed paired runs from the same evidence revision, initial conditions, and
  horizon. Map relevant board commands through trusted configured bindings;
  unsupported command effects become explicit model limitations. The LLM selects
  scenario IDs and bounded parameters, never patches or executable paths.
- Return run ID, input versions, assumptions, horizon, modeled action, success,
  and metrics. Start with peak/final temperature and an independently computed
  observed trend; add time-outside-limit simulation metrics when time-series
  units and sampling semantics are established.
- Keep simulation available until its run budget is exhausted. Track attempts
  separately from successful relevant results. A failed run cannot satisfy a
  simulation prerequisite; preserve its failure as evidence.
- Require configured thermal evidence for a simulation-backed conclusion.
  A power-only run remains supporting evidence. Permit an inconclusive assessment
  when the model is unavailable or does not resolve the question.
- Add an injectable scenario runner so multi-run behavior can be tested without
  an EDS installation. Keep `SedaroSimulator` as the production implementation.

**Exit checks:** baseline/recovery runs share input provenance; modeled actions
are distinct; failed/power-only runs do not satisfy thermal requirements;
comparisons remain available after the first run; timeout/cancellation and metric
units/missing values are validated.

**External dependency:** verify the actual EDS thermal output fields, initialization
bindings, command schedule inputs, and time units before authoring the live
thermal scenario. The current power-only fixture does not supply these.
Deterministic development can proceed with synthetic thermal results meanwhile.

## 5. Conditional Recovery and Proposal Deduplication

**Files:** `src/planner.rs`, `src/coordinator.rs`, `src/config.rs`,
`src/evidence.rs`.

- Expose `select_recovery_action` only for a current `thermal_anomaly` assessment
  requesting recovery evaluation. Require the assessment ID, candidate/action,
  expected benefit, precondition evidence, and applicable simulation run IDs.
- Validate configured actions, modeled-action/baseline matches, successful thermal
  evidence where required, freshness, and material revision. Encode critical
  preconditions/thermal constraints as machine-checkable configuration; descriptive
  precondition text alone is not executable validation.
- Check existing relevant board commands. Suppress equivalent pending/approved
  recovery proposals, account for rejections, and flag contradictory actions.
  Track the local proposal intent until the board reflects it to cover that gap.
- Submit through the existing SAFE command board only after coordinator validation.
  Record assessment ID, evidence revision, and proposal disposition in the audit
  event. Monitor subsequent telemetry for response; board approval alone does
  not close an episode.
- If no action is justified, complete with monitoring or operator review and
  explain the unmet condition. This remains a valid anomaly assessment.

**Exit checks:** justified recovery submits once; missing preconditions, obsolete
evidence, failed required simulation, duplicate recovery, and unresolved command
conflicts produce no new command and an explicit disposition.

## 6. End-to-End Fixtures, Configuration, and Documentation

**Files:** `tests/static_profile_integration.rs`, new
`tests/thermal_assessment_integration.rs`,
`tests/live_openai_simulation_e2e.rs`, `testdata/`, `README.md`,
`../safe/autonomy_mode_config.json`; adapter tests if tool schemas require changes.

- Add deterministic transport-level scenarios for true anomaly, explained
  transient/recovery, insufficient evidence, board-driven explanation/conflict,
  simulation failure, and stale-plan cancellation. Assert assessment events and
  absence/presence of commands, rather than merely accepting an eligible action.
- Extend the opt-in live test to supply board context and verify the diagnostic
  evidence and successful thermal run provenance. Keep provider/EDS-dependent
  verification separate from the deterministic suite.
- Publish an assessment-only fixture and a thermal baseline/recovery fixture
  once the EDS mapping is verified. Use deployment-supplied EDS paths. Document
  migration from single-action automatic emission, new budgets/configuration,
  assessment records, and how to follow the episode logs.

**Checks during implementation:**

```bash
cargo test -p mode-anomaly-recovery
cargo test -p safe-llm-adapter
```

Run the repository-required build and workspace tests at integration completion:

```bash
cargo build --workspace
cargo test --workspace
```

## Completion Checklist

| Story acceptance criterion | Delivered by |
| --- | --- |
| 1. Assessment precedes recovery | Milestones 1, 5 |
| 2. Telemetry and board considered together | Milestone 2 |
| 3. Evidence survives tool calls | Milestones 2, 4 |
| 4. Simulation tests the thermal hypothesis | Milestones 4, 6 and verified EDS mapping |
| 5. Actionable uncertainty | Milestones 1, 2, 4 |
| 6. Current evidence and reassessment | Milestone 3 |
| 7. Explicit recovery basis | Milestone 5 |
| 8. Auditable result | Milestones 1–6 |

Each milestone includes its focused deterministic tests. Full-story completion
requires the real thermal-model integration as well as the assessment-only
delivery; a successful power simulation or emitted command is not sufficient.

## Implementation Status

The assessment contract is implemented with explicit outcomes and dispositions,
including assessment-only configurations with no action catalog. The native tool
loop requires telemetry and board attempts before completion, rebuilds its prompt
from a bounded cumulative evidence ledger, and validates cited evidence and
candidate IDs. Telemetry history is retained per source with duplicate and
out-of-order samples excluded from history-based trend use.

Pending planner work is invalidated when telemetry or board input changes, when
the trigger clears, and on deactivation/shutdown/reconfiguration. The runtime no
longer emits the former automatic single-action proposal. Recovery selection is
available only after a `thermal_anomaly` assessment requests recovery evaluation
and references that assessment ID.

The checked-in EDS scenario is still power-only. No live thermal integration
claim is made: verified EDS thermal output names, initialization bindings,
command-schedule bindings, and time-unit semantics are required before a thermal
baseline/recovery fixture can be configured or live-tested. Synthetic/deterministic
assessment coverage can proceed without those deployment-supplied model details.
