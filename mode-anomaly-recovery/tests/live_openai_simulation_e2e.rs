use std::time::Duration;

use safe::protocol::{
    AutonomyModeBoardState, AutonomyModeId, AutonomyModeInput, AutonomyModeOutput, Command,
    ModeToSafe, SafeToMode, TimedCommand,
};
use safe::telemetry_frame::TelemetryFrame;
use safe::transports::Transport;
use safe::transports::unix::UnixTransport;
use std::process::Stdio;
use tempfile::tempdir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command as TokioCommand;
use tokio::time::timeout;
use uuid::Uuid;

const AUTONOMY_CONFIG: &str = include_str!("../../safe/autonomy_mode_config.json");

fn high_temperature_frame(ts_mono: u64) -> TelemetryFrame {
    TelemetryFrame {
        source: Some("example".to_string()),
        ts_mono,
        payload: serde_json::json!({"telemetry": {"temperature_c": 50.0}}),
    }
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY and the Juno EDS workspace"]
async fn live_openai_tool_calls_run_assessment_and_post_selection_juno_viability() {
    assert!(
        std::env::var_os("OPENAI_API_KEY").is_some(),
        "OPENAI_API_KEY is required for this live test"
    );
    let mut mode_config = serde_json::from_str::<serde_json::Value>(AUTONOMY_CONFIG)
        .expect("autonomy config should be valid JSON")
        .as_array()
        .and_then(|entries| entries.first())
        .and_then(|entry| entry.get("mode_config"))
        .cloned()
        .expect("anomaly recovery mode config should be present");
    mode_config["decision_trace"] = serde_json::json!(true);
    mode_config["max_prompt_chars"] = serde_json::json!(12000);
    mode_config["llm"]["max_output_tokens"] = serde_json::json!(2048);
    mode_config["goal"] = serde_json::json!(
        "When the configured thermal anomaly is confirmed, request recovery evaluation for one eligible action so host code can validate command viability."
    );
    mode_config["analysis_instructions"] = serde_json::json!(
        "After reading telemetry and board evidence, complete the assessment. For this persistent fake anomaly use thermal_anomaly with evaluate_recovery; do not use operator_review unless required evidence is unavailable."
    );
    let mode_id = AutonomyModeId(Uuid::from_u128(2));
    let temp_dir = tempdir().expect("temporary directory");
    let socket_path = temp_dir.path().join("mode_anomaly_recovery.sock");
    let config_path = temp_dir.path().join("mode_config.json");
    tokio::fs::write(
        &config_path,
        serde_json::to_vec(&mode_config).expect("mode config should serialize"),
    )
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
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start advisor mode");
    let stdout = child.stdout.take().expect("mode stdout");
    let stderr = child.stderr.take().expect("mode stderr");
    let logs = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
    let stdout_logs = std::sync::Arc::clone(&logs);
    let stderr_logs = std::sync::Arc::clone(&logs);
    let stdout_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut logs = stdout_logs.lock().await;
            logs.push_str(&line);
            logs.push('\n');
        }
    });
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut logs = stderr_logs.lock().await;
            logs.push_str(&line);
            logs.push('\n');
        }
    });

    let mut stream = timeout(Duration::from_secs(10), server.accept())
        .await
        .expect("mode connection timeout")
        .expect("mode connection failed");
    stream
        .write(SafeToMode::Hello {
            expected_mode: mode_id,
        })
        .await
        .expect("send hello");
    let hello = timeout(Duration::from_secs(10), stream.read())
        .await
        .expect("mode hello timeout")
        .expect("mode hello failed");
    assert!(matches!(hello, ModeToSafe::Hello { mode, .. } if mode == mode_id));

    stream
        .write(SafeToMode::Input(AutonomyModeInput::BoardSnapshot(
            AutonomyModeBoardState::default(),
        )))
        .await
        .expect("send board snapshot");
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
        let output = timeout(Duration::from_secs(90), stream.read())
            .await
            .expect("advisor output timeout")
            .expect("advisor output failed");
        if let ModeToSafe::Output(AutonomyModeOutput::Command(envelope)) = output {
            emitted_command = Some(envelope.cmd);
            break;
        }
    }
    let command_ok = matches!(
        emitted_command,
        Some(TimedCommand::Now(
            Command::PointSunYaw | Command::PointNadir
        ))
    );

    stream
        .write(SafeToMode::Input(AutonomyModeInput::Shutdown))
        .await
        .expect("shutdown advisor");
    timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("advisor shutdown timeout")
        .expect("advisor process wait");
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    let _ = child.kill().await;

    let logs = logs.lock().await;
    assert!(command_ok, "no recovery command emitted ({emitted_command:?}); mode logs:\n{logs}");
    assert!(logs.contains("thermal assessment completed"), "assessment log missing:\n{logs}");
    assert!(logs.contains("simulation_outputs"), "simulation output log missing:\n{logs}");
    assert!(
        logs.contains("power-only recovery viability passed; thermal benefit remains unverified"),
        "power-only thermal separation log missing:\n{logs}"
    );
}
