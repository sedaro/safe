use std::time::Duration;

use safe::protocol::{
    AutonomyModeBoardState, AutonomyModeId, AutonomyModeInput, AutonomyModeOutput, ModeToSafe,
    SafeToMode,
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
async fn persisted_static_anomaly_does_not_auto_emit_a_single_configured_action() {
    profile_without_provider_cannot_execute(PROFILE_FIXTURE).await;
}

#[tokio::test]
async fn shutdown_profile_requires_assessment_and_simulation_over_mode_transport() {
    profile_without_provider_cannot_execute(include_str!("../testdata/shutdown_profile.json"))
        .await;
}

async fn profile_without_provider_cannot_execute(profile: &str) {
    let mode_id = AutonomyModeId(Uuid::from_u128(1));
    let temp_dir = tempdir().expect("temporary directory");
    let socket_path = temp_dir.path().join("mode_anomaly_recovery.sock");
    let config_path = temp_dir.path().join("mode_config.json");
    let mut config: serde_json::Value = serde_json::from_str(profile).unwrap();
    // Hold the port without serving requests: no real model endpoint is used.
    let unavailable_provider = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    config["llm"]["adapter"]["config"]["endpoint"] = serde_json::json!(format!(
        "http://{}/api/generate",
        unavailable_provider.local_addr().unwrap()
    ));
    config["llm"]["request_timeout_ms"] = serde_json::json!(20);
    tokio::fs::write(&config_path, serde_json::to_vec(&config).unwrap())
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
        .arg("--working-directory")
        .arg(temp_dir.path())
        .kill_on_drop(true)
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

    stream
        .write(SafeToMode::Input(AutonomyModeInput::BoardSnapshot(
            AutonomyModeBoardState::default(),
        )))
        .await
        .expect("send board");
    for ts_mono in [1, 2] {
        let mut frame = high_temperature_frame(ts_mono);
        frame.payload["telemetry"]["temperature_c"] = serde_json::json!(70.0);
        stream
            .write(SafeToMode::Input(AutonomyModeInput::Telemetry(frame)))
            .await
            .expect("send telemetry");
    }
    stream
        .write(SafeToMode::Input(AutonomyModeInput::Activate))
        .await
        .expect("activate advisor");

    // The fixture points at no local model. Assessment-first behavior must not
    // replace that unavailable evidence with the former single-action shortcut.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    while let Ok(message) = tokio::time::timeout_at(deadline, stream.read()).await {
        let message = message.expect("mode stays connected");
        if let ModeToSafe::Output(output) = message {
            assert!(!matches!(
                output,
                AutonomyModeOutput::Command(_) | AutonomyModeOutput::Fault(_)
            ));
        }
    }
    assert!(!temp_dir.path().join("shutdown-attempt.jsonl").exists());

    stream
        .write(SafeToMode::Input(AutonomyModeInput::Shutdown))
        .await
        .expect("shutdown advisor");
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
    let _ = child.kill().await;
}
