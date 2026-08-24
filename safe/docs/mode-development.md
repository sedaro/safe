# Mode Development

An autonomy mode is a separate executable supervised by SAFE. A mode receives
telemetry, lifecycle changes, board snapshots, and shutdown messages through
the public `safe` library. It sends commands, board cancellations, lifecycle
states, faults, and heartbeats back to SAFE.

The public mode API is in `safe::mode_runtime`:

- `ModeHandler<C>` defines the mode callbacks.
- `ModeRuntime` provides mode identity, active state, working directory, and
  output helpers.
- `run_mode` loads mode configuration, connects to SAFE, performs the handshake,
  and runs the message loop.
- `ModeOutputTx` can send outputs from a cloned handle.

## Minimal Handler

The following is a minimal mode implementation. The binary must include the
`safe`, `anyhow`, `async-trait`, `serde`, `serde_json`, and `tokio` dependencies.

```rust
use anyhow::Result;
use async_trait::async_trait;
use safe::mode_runtime::{ModeHandler, ModeRuntime, run_mode};
use safe::protocol::{Command, CommandEnvelope, TimedCommand};
use safe::telemetry_frame::TelemetryFrame;
use serde::Deserialize;

#[derive(Default, Deserialize)]
struct ModeConfig {
    #[serde(default = "default_threshold")]
    threshold_c: f64,
}

fn default_threshold() -> f64 {
    34.0
}

#[derive(Default)]
struct ExampleMode {
    config: ModeConfig,
}

#[async_trait]
impl ModeHandler<ModeConfig> for ExampleMode {
    fn set_config(&mut self, config: ModeConfig) -> Result<()> {
        self.config = config;
        Ok(())
    }

    async fn on_telemetry(
        &mut self,
        runtime: &mut ModeRuntime,
        telemetry: TelemetryFrame,
    ) -> Result<()> {
        if !runtime.is_active() {
            return Ok(());
        }

        let Some(value) = telemetry
            .payload
            .get("telemetry")
            .and_then(|value| value.get("temperature_value_c"))
            .and_then(serde_json::Value::as_f64)
        else {
            return Ok(());
        };

        if value > self.config.threshold_c {
            runtime
                .command(CommandEnvelope {
                    from: runtime.mode_id(),
                    cmd: TimedCommand::Now(Command::PointNadir),
                })
                .await?;
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    run_mode::<ModeConfig, _>(ExampleMode::default()).await
}
```

`ModeHandler<C>` requires `C: DeserializeOwned + Default + Send + 'static`.
`set_config` is called once before the IPC connection is established. A config
parse or validation error prevents the mode from starting.

## Callback Semantics

| Callback | When it runs |
| --- | --- |
| `on_activate` | SAFE selects the mode as desired active. |
| `on_deactivate` | SAFE switches away from the mode or receives a deactivation request. |
| `on_telemetry` | Every telemetry frame is forwarded to every connected mode, including inactive modes. Check `runtime.is_active()` before proposing commands. |
| `on_board_snapshot` | SAFE sends the current board to all connected modes. |
| `on_shutdown` | SAFE removes a mode, stops it, or shuts down the daemon. |

Returning an error from a callback sends a fault when possible and exits the
mode runtime. The supervisor may restart the process. A `Restart` input invokes
deactivation and activation callbacks in sequence.

## Output Helpers

Use `ModeRuntime` for common outputs:

```rust
runtime.command(CommandEnvelope {
    from: runtime.mode_id(),
    cmd: TimedCommand::Scheduled {
        cmd: Command::PointSunYaw,
        gps_time: 123456.0,
    },
}).await?;
```

Other outputs include `fault`, `cancel_board`, `lifecycle`, and
`send_output`. SAFE assigns board command IDs when it records a command
proposal. A mode can cancel an existing board ID from a board snapshot.

## Configuration Delivery

For each configured mode, SAFE writes the mode-specific `mode_config` JSON to:

```text
<base>/state/modes/<mode-uuid>/mode-config.json
```

SAFE starts the executable with these arguments:

```text
--endpoint <path-to-ipc.sock>
--config <path-to-mode-config.json>
--mode-id <uuid>
--working-directory <mode-working-directory>
```

Custom `args` from the outer mode configuration are preserved. A mode can also
be run directly by supplying `--endpoint`, `--config`, and `--mode-id`, or by
setting `SAFE_MODE_ENDPOINT`, `SAFE_MODE_ID`, and `SAFE_MODE_WORKING_DIRECTORY`.

The wire details and compatibility rules are documented in
[`mode-protocol.md`](./mode-protocol.md).

## Testing a Mode

Use `MpscTransport` or `TestTransport` for unit tests and `UnixTransport` for
an integration test. The advisor integration test demonstrates the complete
handshake and command-output flow:

```bash
cargo test -p mode-anomaly-recovery --test static_profile_integration
```

Do not use the empty checked-in autonomy-mode configuration as a mode test. Add
a fixture with a real executable path or launch the mode under a test harness.
