use std::time::Duration;

use safe::protocol::{
    AutonomyModeId, AutonomyModeInput, AutonomyModeOutput, Command, ModeToSafe, SafeToMode,
    TimedCommand,
};
use safe::telemetry_frame::TelemetryFrame;
use safe::transports::Transport;
use safe::transports::unix::UnixTransport;
use tempfile::tempdir;
use tokio::process::Command as TokioCommand;
use tokio::time::timeout;
use uuid::Uuid;

const PROFILE_FIXTURE: &str = include_str!("../testdata/static_nominal_profile.json");

fn high_temperature_frame(ts_mono: u64) -> TelemetryFrame {
    TelemetryFrame {
        source: Some("example".to_string()),
        ts_mono,
        payload: serde_json::json!({
            "telemetry": {
                "temperature_c": 50.0,
                "mode": "nominal",
                "enabled": true
            }
        }),
    }
}

#[tokio::test]
async fn persisted_static_anomaly_emits_configured_action_without_ollama() {
    let mode_id = AutonomyModeId(Uuid::from_u128(1));
    let temp_dir = tempdir().expect("temporary directory");
    let socket_path = temp_dir.path().join("mode_anomaly_recovery.sock");
    let config_path = temp_dir.path().join("mode_config.json");
    tokio::fs::write(&config_path, PROFILE_FIXTURE)
        .await
        .expect("write mode config");

    let mut server =
        UnixTransport::<ModeToSafe, SafeToMode>::new(socket_path.to_string_lossy().as_ref())
            .await
            .expect("create mode socket");
    let mode_bin = std::env::var_os("CARGO_BIN_EXE_mode_anomaly_recovery")
        .expect("cargo should provide the advisor binary");
    let mut child = TokioCommand::new(mode_bin)
        .arg("--endpoint")
        .arg(socket_path.to_string_lossy().to_string())
        .arg("--config")
        .arg(config_path.to_string_lossy().to_string())
        .arg("--mode-id")
        .arg(mode_id.to_string())
        .spawn()
        .expect("start advisor mode");

    let mut stream = timeout(Duration::from_secs(5), server.accept())
        .await
        .expect("mode connection timeout")
        .expect("mode connection failed");
    stream
        .write(SafeToMode::Hello {
            expected_mode: mode_id,
        })
        .await
        .expect("send hello");
    let hello = timeout(Duration::from_secs(5), stream.read())
        .await
        .expect("mode hello timeout")
        .expect("mode hello failed");
    assert!(matches!(
        hello,
        ModeToSafe::Hello {
            mode,
            protocol_version: safe::protocol::AUTONOMY_MODE_PROTOCOL_VERSION
        } if mode == mode_id
    ));

    for ts_mono in [1, 2] {
        stream
            .write(SafeToMode::Input(AutonomyModeInput::Telemetry(
                high_temperature_frame(ts_mono),
            )))
            .await
            .expect("send telemetry");
    }
    stream
        .write(SafeToMode::Input(AutonomyModeInput::Activate))
        .await
        .expect("activate advisor");

    let mut emitted_command = None;
    for _ in 0..6 {
        let output = timeout(Duration::from_secs(5), stream.read())
            .await
            .expect("advisor output timeout")
            .expect("advisor output failed");
        if let ModeToSafe::Output(AutonomyModeOutput::Command(envelope)) = output {
            emitted_command = Some(envelope.cmd);
            break;
        }
    }
    assert!(matches!(
        emitted_command,
        Some(TimedCommand::Now(Command::PointSunYaw))
    ));

    stream
        .write(SafeToMode::Input(AutonomyModeInput::Shutdown))
        .await
        .expect("shutdown advisor");
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
    let _ = child.kill().await;
}
