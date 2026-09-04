# Autonomy Mode Protocol

The SAFE-to-mode protocol is version `2` and is defined by the serializable
types in `safe::protocol`.

## Transport

SAFE creates one Unix socket per mode:

```text
<base>/state/modes/<mode-uuid>/ipc.sock
```

Messages use a length-delimited frame followed by bincode serialization. The
transport is local to the host. TCP, MPSC, and test transports exist in the
library for reusable transport tests, but SAFE's mode supervisor currently uses
Unix sockets.

## Handshake

The connection sequence is:

1. SAFE accepts the Unix connection.
2. SAFE sends `SafeToMode::Hello { expected_mode }`.
3. The mode verifies the UUID and sends
   `ModeToSafe::Hello { mode, protocol_version: 2 }`.
4. SAFE marks the mode connected and sends `Activate` if it is the desired
   active mode.
5. The mode emits `Lifecycle { state: "ready" }` after its runtime is ready.

A wrong mode ID or protocol version is rejected and the supervisor retries the
connection. The protocol version is not negotiated; both sides must use the
same constant.

## SAFE to Mode

`SafeToMode::Input` wraps one of these values:

| Input | Meaning |
| --- | --- |
| `Telemetry(TelemetryFrame)` | A telemetry frame. It is sent to every connected mode. |
| `Activate` | The mode is the desired active mode. |
| `Deactivate` | The mode is no longer selected. |
| `Restart` | Run the mode's deactivate/activate lifecycle sequence. |
| `Shutdown` | Flush outputs, run `on_shutdown`, and exit. |
| `BoardSnapshot(AutonomyModeBoardState)` | Current proposals and gatekeeper decisions. |

`TelemetryFrame` contains optional `source`, a `ts_mono` value, and a JSON
`payload` value. Protocol serialization represents the payload as a JSON string
inside the serialized frame.

## Mode to SAFE

`ModeToSafe::Output` wraps one of these values:

| Output | Meaning |
| --- | --- |
| `Command(CommandEnvelope)` | Propose a `TimedCommand` to the command board. |
| `CancelBoard { id, reason }` | Cancel a board proposal. |
| `Fault(String)` | Report an unrecoverable mode error. |
| `Lifecycle { state }` | Report `ready`, `active`, `inactive`, or `stopping`. |
| `Heartbeat` | Prove the mode process is responsive. |

`CommandEnvelope.from` must identify the mode that produced the command. A
`TimedCommand` is either `Now(Command)`, `NOOP`, or
`Scheduled { cmd, gps_time }`.

Pointing and payload command variants include `PointNadir`, `PointSunYaw`,
`PointQuaternion`, `Track { latitude_deg, longitude_deg, altitude_m }`,
`PointNadirWithSensor { sensor }`, `PointYpr { roll_deg, pitch_deg, yaw_deg }`,
and `CaptureImage`. `TimedCommand` determines whether any command is immediate
or scheduled; command variants do not perform frame or unit conversion.

## Lifecycle and Heartbeats

`run_mode` sends a `ready` lifecycle output after the handshake. It changes its
local active flag and invokes the relevant callback when it receives
`Activate`, `Deactivate`, or `Restart`. It sends a heartbeat every five seconds.

SAFE marks a connected mode `unresponsive` after more than fifteen seconds
without a heartbeat. The current status is observable through `safectl status`
and `safectl get modes`; unresponsive state does not yet automatically change
mode selection.

## Reconnects and Shutdown

The mode supervisor retries failed socket creation, process startup, handshake,
and connection reads with exponential backoff from 250 milliseconds up to ten
seconds. A normal mode exit is restarted by the sandbox unless it exits
successfully or exceeds `SAFE_SANDBOX_MAX_RESTARTS`.

Shutdown is sent as an input. The mode runtime invokes `on_shutdown`, emits a
stopping lifecycle message, flushes queued outputs, and exits. A mode should
not assume that an output sent after shutdown will be accepted.

## Compatibility and Safety

The router currently forwards outputs without enforcing the intended
active-mode-only filter. Mode authors must still guard command generation with
`ModeRuntime::is_active()`. Protocol compatibility does not provide command
approval; proposals still pass through SAFE's board and gatekeeper paths.
