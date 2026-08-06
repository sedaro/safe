# Sedaro Autonomy Framework for Edge (`safe`)

Sedaro Autonomy Framework for Edge (SAFE) is a Rust workspace for running
telemetry-driven autonomy modes as separate processes and coordinating their
proposals through a command board. SAFE is under active development.

## Current Scope

The current checkout provides:

- A Flight Director-style SAFE daemon with telemetry-driven mode selection.
- Immediate and hysteretic activation rules.
- Out-of-process autonomy modes with Unix-socket IPC, lifecycle messages,
  heartbeats, restart supervision, and resource monitoring.
- A command board with configurable gatekeeper adapters.
- A local JSONL Unix-socket ingress used by `safectl`.
- `safectl` commands for status, modes, telemetry, logs, board state, and mode
  resource usage.
- The `mode-llm-advisor` mode and the `safe-time` utility crate.

The `Service` topology, integrated Sedaro EDS workflow, and simulation/optimization
kits described in older project material are not complete features of this
checkout. See [`Roadmap.md`](./Roadmap.md) for current status and limitations.

SAFE is not a real-time flight software replacement. It consumes telemetry,
selects and supervises autonomy modes, and records or publishes command-board
outputs for integration with host software. The current default platform setup
is a local demonstration adapter, not a flight-ready command-and-control
integration.

## Architecture

The runtime flow is:

1. A telemetry adapter produces `TelemetryFrame` values.
2. SAFE records telemetry and evaluates mode activation rules.
3. The router starts configured mode binaries and forwards telemetry and board
   snapshots to them.
4. An active mode proposes `TimedCommand` values or cancels board proposals.
5. SAFE applies the configured gatekeeper decision to pending proposals.
6. The platform egress writes command status and scheduled command output.

The default configuration uses an example telemetry generator, a local
`safectl` Unix socket, and a disabled gatekeeper that automatically approves
pending batches. Do not use that configuration as a flight safety policy.

## Getting Started

Run these commands from the repository root. The checked-in `run.sh` exports
the repository configuration paths and starts SAFE with the example telemetry
adapter:

```bash
./run.sh
```

The checked-in `safe/autonomy_mode_config.json` is empty, so this starts the
daemon without autonomy modes. Use the following commands in another terminal
to inspect the process:

```bash
cargo run -p safectl -- status
cargo run -p safectl -- logs
cargo run -p safectl -- watch telemetry
```

Send a payload-only telemetry frame through the local ingress socket:

```bash
cargo run -p safectl -- send telemetry --json '{"telemetry":{"temperature_value_c":34.5}}'
```

Payload-only ingress has no `source` and a zero monotonic timestamp. For a
source-bearing frame, use the full JSON ingress shape documented in
[`safectl.md`](./safe/docs/safectl.md), or configure an external telemetry
adapter.

Stop SAFE with `Ctrl-C`. Configuration path resolution, adapters, and runtime
directories are documented in [`runtime-config.md`](./safe/docs/runtime-config.md).

## Development

The repository pins Rust `1.94.1` in `rust-toolchain.toml`.

```bash
cargo build --workspace
cargo test --workspace
```

To develop an autonomy mode, implement the public `ModeHandler` contract and
launch it through `run_mode`. Start with
[`mode-development.md`](./safe/docs/mode-development.md) and
[`mode-protocol.md`](./safe/docs/mode-protocol.md). The LLM advisor has its
own configuration and fixture documentation in
[`mode-llm-advisor/README.md`](./mode-llm-advisor/README.md).

## Documentation

- [`Runtime configuration`](./safe/docs/runtime-config.md)
- [`Activation configuration`](./safe/docs/activation-config.md)
- [`safectl reference`](./safe/docs/safectl.md)
- [`Mode development`](./safe/docs/mode-development.md)
- [`Mode protocol`](./safe/docs/mode-protocol.md)
- [`Runtime operations`](./safe/docs/runtime-operations.md)
- [`Contributor guide`](./CONTRIBUTING.md)
- [`Roadmap and limitations`](./Roadmap.md)
- [`safe-time API`](./safe-time/README.md)

## Workspace Layout

- [`safe/`](./safe/): SAFE library, daemon, runtime, router, transports, and
  platform adapters.
- [`safectl/`](./safectl/): CLI for interacting with a running SAFE daemon.
- [`safe-time/`](./safe-time/): GPS, UTC, MJD, and attitude utility functions.
- [`mode-llm-advisor/`](./mode-llm-advisor/): static nominal-profile advisor
  mode and fixtures.
- [`run.sh`](./run.sh): local development launch script.

## Safety and Deployment Notes

The repository contains unfinished integration paths. In particular, the
default disabled gatekeeper approves command batches, scheduled commands are
written to `out/commands.csv`, and direct `ExecuteNow` host dispatch is not
fully wired through the platform egress. Review
[`runtime-operations.md`](./safe/docs/runtime-operations.md) before treating
SAFE output as an operational command stream.

The current implementation uses Tokio, filesystem state, subprocesses, and
Linux namespace support where available. It is not currently a `no_std` or MCU
target.

## License

This project is licensed under the [Apache-2.0 License](./LICENSE).
