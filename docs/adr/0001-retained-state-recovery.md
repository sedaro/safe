# ADR-0001: Recover SAFE from retained state

- **Status:** Accepted
- **Date:** 2026-10-14

## Context

SAFE can retain an internal `NOOP` handoff after restart. A downstream mode waits
for unresolved upstream proposals; counting a no-effect handoff as unresolved can
stop the mode cycle. Separately, a stopped or silent gatekeeper could leave a
physical proposal pending indefinitely.

Recovery also had an event/effect crash window: the checkpoint could advance even
when the corresponding command-board effect was missing. A retry triggered by new
telemetry after SAFE restarts addresses a lost in-flight request, but does not
correct a downstream mode that is blocked by a pending `NOOP`, supervise a gatekeeper
failure while SAFE remains running, or repair stale restored board state.

## Decision

- Keep `NOOP` on SAFE's normal board/gatekeeper path so the existing accepted
  schedule validation is preserved. Exclude no-effect `NOOP` entries from the
  downstream mode's unresolved-host-command wait predicate.
- Rebuild derived board caches and repair missing board effects from retained
  events before SAFE resumes normal processing. Replay must not execute commands.
- Supervise the external gatekeeper process from SAFE with bounded input writes,
  simulation-budgeted response deadlines, restart backoff, and fail-closed retry.
- Bind a response to its request and evaluated board. Discard it if the relevant
  board state changed while evaluation was in progress.
- Keep runtime state across ordinary patch/restart. Do not use state deletion as
  a recovery procedure.

This changes SAFE and the one downstream mode with the upstream-proposal wait
predicate. Protocol version and gatekeeper-server executables remain unchanged.
No Cargo dependency or lockfile update is needed.

## Consequences and remaining limits

- Pending no-effect handoffs cannot block downstream mode progress, while their
  gatekeeper evaluation of the accepted schedule remains in place. A gatekeeper
  process failure clears the in-flight request so pending proposals can be retried;
  if validation remains unavailable, physical proposals remain pending.
- Physical commands remain pending unless a valid gatekeeper approval is received.
- Event repair works only while its source event remains retained; compaction can
  remove evidence needed to reconstruct a missing historical effect.
- The default response deadline is derived from simulator limits and can exceed
  command lead time. This patch does not reject a late scheduled command. Measure
  target runtimes and verify host-side expiry behavior before deployment.
- SAFE owns and kills the direct gatekeeper child. Any workers it launches must
  honor child shutdown; process-group signaling is not added.
- Approval, host acceptance, and spacecraft execution are distinct. Execution
  requires host acknowledgement and telemetry evidence.
- Clearing input/output/state may temporarily hide a particular stale board but
  does not fix the recovery logic. It can erase accepted-command history and
  recovery deadlines while host reservations remain.

## Alternatives considered

- **Clear retained state:** rejected; destructive and does not fix the failure
  mechanism.
- **Approve physical work when validation times out:** rejected; unavailable
  validation must fail closed.
- **Change every autonomy mode:** rejected. Only the downstream mode that waits
  on upstream proposals needs to distinguish host commands from `NOOP`; SAFE
  retains ownership of the board, persistence, and gatekeeper lifecycle.
