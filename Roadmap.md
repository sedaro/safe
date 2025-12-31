Current state: `safe` works and is robust but implementation is uglier than I'd like.

The work completed to date has aimed to:
- Architect/scaffold
- Determine opinions and areas of flexibility
- Focus on quality, value-add features from day-one to further catalyze adoption
- Focus demo on the trust piece


## Depends On 

- [ ] 🔴 Need SimVM TS to be publicly installable crate so that GitHub Actions pipelines can properly `cargo test`
- [ ] 🔴 Need Sedaro type system in simvm crate to support keying into datum via string keys, otherwise we are majorly at risk of setting the wrong SV
- [ ] 🔴 `pid_config` not getting sourced from the init file in satops sim
	- SAFE commit: ad4f084b98541f9aa393f4021c228766cc284410
	- scf commit: e6bf8a283d9d1d58905d3222923f163c0bf36198
- [ ] 🔴 How to have multiple init files without racing?

## Discovery

- [ ] Format as proper crate
- [ ] Run SAFE ASAP on MCU and figure out whether this is practical.  Will sims run on an MCU?  Do sims require std?  Is `no-std` even possible
- [ ] Scaffold out `Service` topology kind with as much reuse from `Flight` as possible.

## Cleanup

- [ ] Fix logging, including moving the tracing setup into flight
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
- [ ] Simplement engagement mode
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

## Reliability

- [ ] 🔴 There is an issuing with the router loop given hows it's implemented with a tokio::select.  Essentially commands from active modes can be never forwarded because the telemetry received branch runs faster than the active mode command forwarder branch.  If you send many telemetry writes quickly which cause mode switches you'll only get the commands from the last mode activation out.  The intent should be for all commands sent while active to make it out, regardless of telemetry arrival rates.  tokio::select likely doesn't give us the necessary control authority here.  Create a unit test for this where you send several telem messages back to back and then assert that all loopbacks are properly ordered and from the expected mode.
  - This also means that a fault in how frequently telem is sent could completely disable the ability for safe to issue commands
- [ ] Autonomy Mode heartbeat (like k8s readiness) that is configurable
  - If an AM becomes unresponsive and it has a probe defined, Router should deactivate it and allow another mode to activate until the unhealthy mode is healthy again.
- [ ] Observability and Reproducibility Subsystem
  - Figure out if current logging is ugly and non-future proof.  We don't want to miss important logs because of some nested span.  But maybe this is okay because they are still written to the main log.  How do we record logs from subprocesses such that they are still queryable by timestamp??
  - 🔴 Replay needs immediate attention and design before its too late to rollout.
- [ ] Implement autonomy mode restart on error, with backoff?
