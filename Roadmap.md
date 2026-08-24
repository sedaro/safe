# Roadmap and Current Status

SAFE is an active development workspace. This document separates behavior that
exists in the checkout from planned work and known limitations.

## Implemented in This Checkout

- Rust workspace packages for `safe`, `safectl`, `safe-time`, and
  `mode-anomaly-recovery`.
- SAFE daemon startup, persisted flight state, event/output JSONL logs, and
  startup recovery.
- Telemetry-driven immediate and hysteretic mode selection.
- Manual mode activation and deactivation through `safectl`.
- Out-of-process autonomy modes using protocol version 2 over Unix sockets.
- Mode lifecycle messages, five-second heartbeats, reconnect supervision, and
  resource snapshots.
- Runtime mode configuration reload approximately once per second.
- Command board proposals, cancellations, approval/rejection records, and
  external gatekeeper JSONL integration.
- Local `safectl` JSONL ingress and operational status files.
- Static nominal-profile LLM advisor with deterministic single-action behavior,
  constrained Ollama decisions, and integration tests.
- GPS, UTC, MJD, leap-table, and Euler-213 quaternion utilities.

## Partial or Demonstration-Only

- The default `safe/safe.yaml` uses example telemetry and a disabled gatekeeper.
  Disabled gatekeeper mode automatically approves pending batches and is not a
  mission safety policy.
- Scheduled board commands are written to `out/commands.csv`. The direct
  `ExecuteNow` effect is represented in SAFE state but is not fully connected
  to a host command egress.
- `safectl top modes` and sandbox metrics use different default directory
  layouts and need contract cleanup before being treated as reliable tooling.
- The router currently forwards mode outputs without enforcing the intended
  active-mode-only check. This requires a safety decision and regression test.
- The `mode_no_images` integration test exits early when its binary is absent;
  the workspace does not contain that binary.
- `safe/src/kits/optimization.rs` refers to an incomplete simulation API and is
  not part of the public `safe` library surface.

## Known Reliability Work

- Guarantee command delivery and ordering when telemetry arrives faster than
  mode outputs are consumed.
- Enforce that a mode cannot publish commands after it has been deactivated.
- Make heartbeat policy configurable and use unresponsive state in selection.
- Improve replay design, event durability, error handling, and observability.
- Add explicit tests for mode output filtering, reload failure behavior, and
  host dispatch semantics.

## Planned Capabilities

- A supported `Service` topology for command-sequence validation.
- Stable host C2 interoperability and command/telemetry serialization
  semantics.
- A complete EDS/simulation integration and supported simulation kits.
- Engagement and background-mode concepts.
- Better activation language and telemetry filtering/debouncing.
- Formal deployment targets, including a decision about `no_std` or MCU use.
- Cross-instance SAFE collaboration interfaces.

## Documentation and Reproducibility

The supported local workflow is the repository-root `./run.sh` plus the
reference pages under `safe/docs/`. External EDS/SCF notes are intentionally
kept separate from the local quickstart because the required repositories,
scripts, artifacts, and host paths are not present here.
