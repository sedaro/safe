# Contributing

## Toolchain

The repository pins Rust `1.94.1` and the `aarch64-unknown-linux-gnu` and
`aarch64-apple-darwin` targets in `rust-toolchain.toml`. Use the pinned toolchain
for local and CI-compatible results.

## Build and Test

From the repository root:

```bash
cargo build --workspace
cargo test --workspace
```

Useful focused commands are:

```bash
cargo test -p safe
cargo test -p safectl
cargo test -p safe-time
cargo test -p mode-anomaly-recovery
cargo test -p mode-anomaly-recovery --test static_profile_integration
```

The CI workflow runs `cargo build --workspace --verbose` and
`cargo test --workspace --verbose`.

The SAFE `mode_no_images_integration` test currently returns early when the
`mode_stationkeeping` binary is unavailable. A passing result for that test does
not prove that a mode executable was launched.

## Local Runtime

Use `./run.sh` to start the daemon with repository-local paths. The default
configuration uses generated example telemetry, an empty mode list, and a
disabled gatekeeper. Do not use it as a flight configuration.

Use the documentation under `safe/docs/` before changing protocol, runtime
state, or CLI behavior. Changes to serialized protocol types require reviewing
the protocol version and integration fixtures.

## Mode Changes

Modes are separate workspace binaries and should use `safe::mode_runtime::run_mode`.
Add unit tests for handler decisions and a Unix transport integration test for
handshake, lifecycle, telemetry, board, and shutdown behavior.

When adding a mode fixture, keep its binary path relative to the fixture or
document the external build prerequisite. Do not add machine-specific absolute
paths to checked-in configuration.

## Scope and Platform Assumptions

The current implementation depends on `std`, Tokio, filesystem state,
subprocesses, and process metrics. It is not currently `no_std` or MCU-ready.
Linux namespace isolation is optional in `auto` mode and must be explicitly
validated for any deployment that requires it.
