Current state: `safe` works and is robust but implementation is uglier than I'd like.

The work completed to date has aimed to:
- Architect/scaffold
- Determine opinions and areas of flexibility
- Focus on quality, value-add features from day-one to further catalyze adoption
- Focus demo on the trust piece

**WARNING: The following is not comprehensive**

## Depends On 

- [ ] 🔴 `pid_config` not getting sourced from the init file in satops sim
	- SAFE commit: ad4f084b98541f9aa393f4021c228766cc284410
	- scf commit: e6bf8a283d9d1d58905d3222923f163c0bf36198

## Discovery

- [ ] Format as proper crate
- [ ] Run SAFE ASAP on MCU and figure out whether this is practical.  Will sims run on an MCU?  Do sims require std?  Is `no-std` even possible
- [ ] Scaffold out `Service` topology kind with as much reuse from `Flight` as possible.
- Does the client/server model for transports make sense?  Should router take a stream instead of a transport impls?
  - Document: Passing a handle to the modes allow them to handle reconnect
  - A: It does make sense for flexiblity and the opportunity to flag off certain transports which aren't supported on particular platforms.  Also server-broadcast is nice.  Transports enable platform-agnostic IPC.

## Cleanup

- [ ] Fix logging, including moving the tracing setup into Flight
- [ ] Rework C2 interface and handler
  - C2 interface should be a client not a server?
- [ ] Better error handling and reduce unwraps
- [ ] Improve testability
  - Figure out auditable transports, as a continuation of TestTransport wrapper

## Features

- [ ] C2 interoperability and abstraction
  - Use SVv2 for command/telem serde and semantics?  QK support would be amazing.
  - Look into ROS2 `ros2_control` for inspo
  - Look into JPL F Prime [Commands](https://fprime.jpl.nasa.gov/latest/tutorials-hello-world/docs/hello-world/#introduction-and-f-terminology)
  - AFSIM
  - Anduril Lattice
- [ ] Sim Kits
  - [ ] Optimizers
  - [ ] Stats: Brad to architect this module (https://sedaro.slack.com/archives/D03FLBA7WCT/p1765289350221849)
  - ...
- [ ] Implement engagement mode
- [ ] Autonomy Mode reg/dereg and static, persisted state for recovery
- [ ] `safectl` vNext
- [ ] Build out routing logic
  - Third-party options:
		- JsonLogic is very solid to replace what we have - rust support (https://github.com/Bestowinc/json-logic-rs?tab=readme-ov-file)
		- CEL - rust support (https://github.com/cel-rust/cel-rust)
			- Allows for the inclusion of custom functions in the lang
		- rhai - supports no-std and seems capable (https://crates.io/crates/rhai)
		- Skylark - python-esque and has rust interpreter (https://github.com/facebook/starlark-rust)
  - Whatever we choose should probably work for `no-std`
  - Justification for our lang is it's guaranteed to work vs. scripting which isn't guaranteed.  We should offer both.
- IDEA: Way to isolate the exercise/test/develop AMs in isolation without SAFE overhead
- Add activation for when telemetry changes value!
- Support background running modes and foreground
  - Implement a way to have background modes which are alerted when they are activated/deactivated
- Implement autonomy mode transport fault recovery (i.e. reconnect)
  - Unless we come up with somethign more clever, this will require that we implement a handshake to identify the Mode on connect.  This could probably be handle by some connection factory.
- (WIP) Utilities for debouncing or filtering out potentially noisy telemetry inputs to get a confident reading.  Make this part of activations for modes.
- Allow for different SAFE instances to "collaborate" via dedicated interface

## Reliability

- [ ] 🔴 There is an issuing with the router loop given hows it's implemented with a tokio::select.  Essentially commands from active modes can be never forwarded because the telemetry received branch runs faster than the active mode command forwarder branch.  If you send many telemetry writes quickly which cause mode switches you'll only get the commands from the last mode activation out.  The intent should be for all commands sent while active to make it out, regardless of telemetry arrival rates.  tokio::select likely doesn't give us the necessary control authority here.  Create a unit test for this where you send several telem messages back to back and then assert that all loopbacks are properly ordered and from the expected mode.
  - This also means that a fault in how frequently telem is sent could completely disable the ability for safe to issue commands
- [ ] Autonomy Mode heartbeat (like k8s readiness) that is configurable
  - If an AM becomes unresponsive and it has a probe defined, Router should deactivate it and allow another mode to activate until the unhealthy mode is healthy again.
- [ ] Observability and Reproducibility Subsystem
  - Figure out if current logging is ugly and non-future proof.  We don't want to miss important logs because of some nested span.  But maybe this is okay because they are still written to the main log.  How do we record logs from subprocesses such that they are still queryable by timestamp??
  - 🔴 Replay needs immediate attention and design before its too late to rollout.
- [ ] Implement autonomy mode restart on error, with backoff?
- The Transport interface should likely implement a means of acknowledging what has been received
  - This gets more difficult with split streams though
  - Is TCP ack enough?  What about UDP?
- It is important to guarantee that Modes can't issue commands when Router logic would deactivate them

## Tech Debt
- Need better Activation interpreter