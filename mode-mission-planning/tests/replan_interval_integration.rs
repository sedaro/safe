use std::time::Duration;

use safe::protocol::{
    AUTONOMY_MODE_PROTOCOL_VERSION, AutonomyModeId, AutonomyModeInput, AutonomyModeOutput,
    ModeToSafe, SafeToMode,
};
use safe::telemetry_frame::TelemetryFrame;
use safe::transports::Transport;
use safe::transports::unix::UnixTransport;
use tempfile::tempdir;
use tokio::process::Command;
use tokio::time::timeout;
use uuid::Uuid;

#[tokio::test]
async fn failed_planning_does_not_start_replan_interval() {
    let mode_id = AutonomyModeId(Uuid::from_u128(42));
    let directory = tempdir().unwrap();
    let socket = directory.path().join("mission-planning.sock");
    let config = directory.path().join("mode-config.json");
    tokio::fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "eds_path": "/does/not/exist",
            "input_adapter_command": ["/does/not/exist"],
            "planning_horizon_secs": 60.0,
            "min_replan_interval_secs": 300,
            "telemetry_gps_time_pointer": "/gps_time",
            "telemetry_state_of_charge_pointer": "/soc",
            "result_file": "result.jsonl",
            "state_of_charge_field": "soc",
            "low_power_state_of_charge": 0.3,
            "recovered_state_of_charge": 0.6,
            "minimum_elevation_deg": 10.0
        }))
        .unwrap(),
    )
    .await
    .unwrap();

    let mut server =
        UnixTransport::<ModeToSafe, SafeToMode>::new(socket.to_string_lossy().as_ref())
            .await
            .unwrap();
    let binary = std::env::var_os("CARGO_BIN_EXE_mode_mission_planning").unwrap();
    let mut child = Command::new(binary)
        .arg("--endpoint")
        .arg(&socket)
        .arg("--config")
        .arg(&config)
        .arg("--mode-id")
        .arg(mode_id.to_string())
        .spawn()
        .unwrap();
    let mut stream = timeout(Duration::from_secs(5), server.accept())
        .await
        .unwrap()
        .unwrap();
    stream
        .write(SafeToMode::Hello {
            expected_mode: mode_id,
        })
        .await
        .unwrap();
    assert!(matches!(
        stream.read().await.unwrap(),
        ModeToSafe::Hello {
            mode,
            protocol_version: AUTONOMY_MODE_PROTOCOL_VERSION
        } if mode == mode_id
    ));

    stream
        .write(SafeToMode::Input(AutonomyModeInput::Telemetry(
            TelemetryFrame::new(serde_json::json!({"gps_time": 100.0, "soc": 0.8})),
        )))
        .await
        .unwrap();
    stream
        .write(SafeToMode::Input(AutonomyModeInput::Activate))
        .await
        .unwrap();
    read_until(&mut stream, |output| {
        matches!(output, AutonomyModeOutput::Fault(_))
    })
    .await;

    stream
        .write(SafeToMode::Input(AutonomyModeInput::Deactivate))
        .await
        .unwrap();
    stream
        .write(SafeToMode::Input(AutonomyModeInput::Activate))
        .await
        .unwrap();
    read_until(&mut stream, |output| {
        matches!(output, AutonomyModeOutput::Fault(_))
    })
    .await;

    stream
        .write(SafeToMode::Input(AutonomyModeInput::Shutdown))
        .await
        .unwrap();
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
    let _ = child.kill().await;
}

async fn read_until(
    stream: &mut Box<dyn safe::transports::Stream<ModeToSafe, SafeToMode>>,
    predicate: impl Fn(&AutonomyModeOutput) -> bool,
) {
    for _ in 0..10 {
        let message = timeout(Duration::from_secs(5), stream.read())
            .await
            .unwrap()
            .unwrap();
        if let ModeToSafe::Output(output) = message
            && predicate(&output)
        {
            return;
        }
    }
    panic!("expected autonomy mode output was not received");
}
