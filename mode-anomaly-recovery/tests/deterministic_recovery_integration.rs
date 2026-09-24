use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use safe::protocol::{
    AutonomyModeInput, AutonomyModeOutput, CommandEnvelope, ModeToSafe, SafeToMode, TimedCommand,
};
use safe::transports::{Stream, Transport, UnixTransport};
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::time::{Instant, timeout, timeout_at};
use uuid::Uuid;

// Exercise the unchanged production selector with actual mode NOOP output.
pub use safe::protocol::AutonomyModeId;
pub use safe::runtime::AutonomyModeMeta;
pub use safe::telemetry_frame;
#[allow(dead_code)]
#[path = "../../safe/src/definitions.rs"]
mod definitions;
#[allow(dead_code)]
#[path = "../../safe/src/flight.rs"]
mod flight;

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}
fn id(name: &str) -> AutonomyModeId {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, name.as_bytes()).into()
}

fn chain() -> flight::Flight {
    #[derive(serde::Deserialize)]
    struct Routing {
        name: String,
        priority: u8,
        activation: definitions::Activation,
    }
    let routing: Vec<Routing> =
        serde_json::from_str(include_str!("../recovery-routing.example.json")).unwrap();
    let mut flight = flight::Flight::default();
    flight.set_autonomy_modes(
        routing
            .iter()
            .map(|entry| AutonomyModeMeta {
                id: id(&entry.name),
                name: entry.name.clone(),
                priority: entry.priority,
                enabled: true,
            })
            .collect(),
    );
    flight.set_autonomy_mode_activations(
        routing
            .into_iter()
            .map(|entry| flight::AutonomyModeActivation {
                id: id(&entry.name),
                activation: Some(entry.activation),
            })
            .collect(),
    );
    flight.recalculate_active_autonomy_mode();
    flight
}

fn profile() -> Value {
    let mut config: Value = serde_json::from_str(include_str!(
        "../testdata/deterministic_recovery_profile.json"
    ))
    .unwrap();
    config["recovery"]["minimum_hold_secs"] = json!(2);
    config["recovery"]["trigger_samples"] = json!(1);
    config["recovery"]["recovery_samples"] = json!(1);
    config["recovery"]["recovery_dwell_secs"] = json!(0);
    config
}

struct Mode {
    child: Child,
    stream: Box<dyn Stream<ModeToSafe, SafeToMode>>,
}

impl Mode {
    async fn start(directory: &Path, config: &Value) -> Self {
        let config_path = directory.join("mode-config.json");
        tokio::fs::write(&config_path, serde_json::to_vec(config).unwrap())
            .await
            .unwrap();
        let socket = directory.join("mode.sock");
        let _ = tokio::fs::remove_file(&socket).await;
        let mut server = UnixTransport::<ModeToSafe, SafeToMode>::new(socket.to_str().unwrap())
            .await
            .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_mode_anomaly_recovery"))
            .arg("--endpoint")
            .arg(&socket)
            .arg("--config")
            .arg(config_path)
            .arg("--mode-id")
            .arg(id("AnomalyRecovery").to_string())
            .arg("--working-directory")
            .arg(directory)
            .stdout(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stream = timeout(Duration::from_secs(5), server.accept())
            .await
            .unwrap()
            .unwrap();
        stream
            .write(SafeToMode::Hello {
                expected_mode: id("AnomalyRecovery"),
            })
            .await
            .unwrap();
        assert!(matches!(
            stream.read().await.unwrap(),
            ModeToSafe::Hello {
                protocol_version: safe::protocol::AUTONOMY_MODE_PROTOCOL_VERSION,
                ..
            }
        ));
        let mut mode = Self { child, stream };
        // Wait until startup/tick initialization before generating sensor timestamps.
        timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    mode.stream.read().await.unwrap(),
                    ModeToSafe::Output(AutonomyModeOutput::Heartbeat)
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        mode
    }

    async fn input(&mut self, input: AutonomyModeInput) {
        self.stream.write(SafeToMode::Input(input)).await.unwrap();
    }

    async fn telemetry(&mut self, soc: f64, temperature: f64) {
        let at = now();
        self.input(AutonomyModeInput::Telemetry(
            telemetry_frame::TelemetryFrame {
                source: Some("example".into()),
                ts_mono: (at * 1000.0) as u64,
                payload: json!({"battery":{"soc":soc,"measured_at_unix_secs":at,"valid":true},
                "thermal":{"temperature_c":temperature,"measured_at_unix_secs":at,"valid":true}}),
            },
        ))
        .await;
    }

    async fn noop(&mut self) -> CommandEnvelope {
        timeout(Duration::from_secs(5), async {
            loop {
                match self.stream.read().await.unwrap() {
                    ModeToSafe::Output(AutonomyModeOutput::Command(command)) => {
                        assert!(matches!(command.cmd, TimedCommand::NOOP));
                        return command;
                    }
                    ModeToSafe::Output(AutonomyModeOutput::Fault(error)) => {
                        panic!("mode fault: {error}")
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("NOOP timeout")
    }

    async fn silent_for(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while let Ok(message) = timeout_at(deadline, self.stream.read()).await {
            if let ModeToSafe::Output(output) = message.unwrap() {
                assert!(
                    !matches!(
                        output,
                        AutonomyModeOutput::Command(_) | AutonomyModeOutput::Fault(_)
                    ),
                    "unexpected output: {output:?}"
                );
            }
        }
    }

    async fn stop(&mut self) {
        self.child.kill().await.unwrap();
        self.child.wait().await.unwrap();
    }
}

async fn record(directory: &Path) -> Value {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(bytes) = tokio::fs::read(directory.join("recovery-state.json")).await {
                if let Ok(record) = serde_json::from_slice::<Value>(&bytes) {
                    return record;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}

#[test]
fn existing_activation_rules_start_with_anomaly_and_cycle_on_completion() {
    let mut flight = chain();
    assert_eq!(
        flight.get_active_autonomy_mode(),
        Some(id("AnomalyRecovery"))
    );
    for (completed, next) in [
        ("AnomalyRecovery", "MissionPlanning"),
        ("MissionPlanning", "CoorbitalEvasion"),
        ("CoorbitalEvasion", "AnomalyRecovery"),
    ] {
        flight.set_last_planned_autonomy_mode(id(completed));
        flight.recalculate_active_autonomy_mode();
        assert_eq!(flight.get_active_autonomy_mode(), Some(id(next)));
    }
    let mut restarted: flight::Flight =
        serde_json::from_value(serde_json::to_value(&flight).unwrap()).unwrap();
    for _ in 0..10 {
        restarted.recalculate_active_autonomy_mode();
        assert_eq!(
            restarted.get_active_autonomy_mode(),
            Some(id("AnomalyRecovery")),
            "without a NOOP the existing hysteresis retains anomaly across restart"
        );
    }
}

#[tokio::test]
async fn nominal_noop_yields_using_existing_selector_and_is_emitted_once_per_activation() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = profile();
    config["llm"] = json!({"adapter":{"kind":"unavailable-adapter","config":{}},"model":"unused"});
    let mut mode = Mode::start(directory.path(), &config).await;
    mode.telemetry(0.8, 50.0).await;
    mode.silent_for(Duration::from_millis(100)).await; // inactive modes never command
    mode.input(AutonomyModeInput::Activate).await;
    let noop = mode.noop().await;
    let mut selector = chain();
    selector.set_last_planned_autonomy_mode(noop.from);
    selector.recalculate_active_autonomy_mode();
    assert_eq!(
        selector.get_active_autonomy_mode(),
        Some(id("MissionPlanning"))
    );
    mode.silent_for(Duration::from_millis(250)).await;
    mode.input(AutonomyModeInput::Deactivate).await;
    mode.telemetry(0.1, 90.0).await;
    mode.silent_for(Duration::from_millis(150)).await;
    assert!(!directory.path().join("recovery-state.json").exists());
    mode.telemetry(0.8, 50.0).await;
    mode.input(AutonomyModeInput::Activate).await;
    mode.noop().await;
    mode.stop().await;
}

#[tokio::test]
async fn shutdown_restart_waits_original_deadline_and_remains_idle_until_recovered() {
    let directory = tempfile::tempdir().unwrap();
    let config = profile();
    let mut mode = Mode::start(directory.path(), &config).await;
    mode.input(AutonomyModeInput::Activate).await;
    mode.telemetry(0.1, 90.0).await;
    let original = record(directory.path()).await;
    mode.silent_for(Duration::from_millis(200)).await;
    mode.stop().await;

    let mut restarted = Mode::start(directory.path(), &config).await;
    restarted.input(AutonomyModeInput::Activate).await;
    restarted.telemetry(0.8, 50.0).await;
    restarted.silent_for(Duration::from_millis(300)).await;
    assert_eq!(record(directory.path()).await["id"], original["id"]);
    assert_eq!(
        record(directory.path()).await["resume_after"],
        original["resume_after"]
    );
    restarted.telemetry(0.1, 90.0).await;
    restarted.silent_for(Duration::from_secs(2)).await;
    assert_eq!(
        record(directory.path()).await["id"],
        original["id"],
        "persistent anomaly must not repeat shutdown"
    );
    restarted.telemetry(0.8, 50.0).await;
    restarted.noop().await;
    assert!(record(directory.path()).await["completed_at"].is_number());
    restarted.silent_for(Duration::from_millis(150)).await;
    restarted.stop().await;
}

#[tokio::test]
async fn recovered_telemetry_releases_on_timer_without_a_new_frame() {
    let directory = tempfile::tempdir().unwrap();
    let mut mode = Mode::start(directory.path(), &profile()).await;
    mode.input(AutonomyModeInput::Activate).await;
    mode.telemetry(0.1, 90.0).await;
    let episode = record(directory.path()).await;
    mode.telemetry(0.8, 50.0).await;
    mode.noop().await;
    assert!(now() >= episode["resume_after"].as_f64().unwrap());
    mode.stop().await;
}

#[tokio::test]
async fn advisor_is_quiet_during_cooldown_and_provider_failure_does_not_block_noop() {
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = profile();
    config["advisory"] = json!({"enabled":true,"local_inference":false});
    config["llm"] = json!({"adapter":{"kind":"ollama","config":{"endpoint":format!("http://{}/api/generate", listener.local_addr().unwrap())}},
        "model":"test","context_window_tokens":32768,"request_timeout_ms":100});
    let mut mode = Mode::start(directory.path(), &config).await;
    mode.input(AutonomyModeInput::Activate).await;
    mode.telemetry(0.1, 90.0).await;
    record(directory.path()).await;
    assert!(
        timeout(Duration::from_millis(300), listener.accept())
            .await
            .is_err(),
        "no inference during cooldown"
    );
    mode.telemetry(0.8, 50.0).await;
    mode.noop().await;
    let (connection, _) = timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    drop(connection); // deliberately fail the optional post-recovery advisor
    mode.silent_for(Duration::from_millis(250)).await;
    assert!(record(directory.path()).await["completed_at"].is_number());
    mode.stop().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn critical_recovery_cancels_an_in_flight_eds_process_without_waiting_for_it() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let eds = directory.path().join("eds");
    let pid_file = directory.path().join("eds.pid");
    tokio::fs::write(
        &eds,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 60\n",
            pid_file.display()
        ),
    )
    .await
    .unwrap();
    std::fs::set_permissions(&eds, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = profile();
    config["advisory"] = json!({"enabled":true,"local_inference":true});
    config["llm"] = json!({"adapter":{"kind":"ollama","config":{"endpoint":"http://127.0.0.1:1/api/generate"}},"model":"test"});
    let legacy: Value =
        serde_json::from_str(include_str!("../testdata/shutdown_profile.json")).unwrap();
    config["simulation"] = legacy["simulation"].clone();
    config["simulation"]["eds_path"] = json!(eds);
    config["simulation"]["run_timeout_ms"] = json!(60000);
    config["simulation"]["initialization"]["epoch_path"] = json!("epoch_mjd");
    config["simulation"]["initialization"]["patches"][0]["telemetry_path"] = json!("battery.soc");
    config["simulation"]["initialization"]["requirements"][0]["path"] = json!("battery.soc");
    for scenario in config["simulation"]["scenarios"].as_array_mut().unwrap() {
        scenario["state_bindings"][0]["path"] = json!("battery.soc");
    }
    config["nominal_profiles"] = json!([{"id":"warning","source":"example","rules":[
        {"id":"hot","path":"thermal.temperature_c","kind":"number_range","max":60.0,"eligible_actions":[]}
    ]}]);
    let mut mode = Mode::start(directory.path(), &config).await;
    mode.input(AutonomyModeInput::BoardSnapshot(Default::default()))
        .await;
    mode.input(AutonomyModeInput::Activate).await;
    let at = now();
    mode.input(AutonomyModeInput::Telemetry(telemetry_frame::TelemetryFrame {
        source: Some("example".into()), ts_mono: (at * 1000.0) as u64,
        payload: json!({"epoch_mjd":60000.0,"battery":{"soc":0.8,"measured_at_unix_secs":at,"valid":true},
            "thermal":{"temperature_c":70.0,"measured_at_unix_secs":at,"valid":true}}),
    })).await;
    mode.noop().await;
    let pid: u32 = timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(text) = tokio::fs::read_to_string(&pid_file).await {
                if let Ok(pid) = text.trim().parse() {
                    break pid;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("EDS should start for noncritical advisory assessment");
    struct EdsCleanup(u32);
    impl Drop for EdsCleanup {
        fn drop(&mut self) {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &self.0.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    let mut cleanup = Some(EdsCleanup(pid));
    mode.input(AutonomyModeInput::Deactivate).await;
    mode.telemetry(0.1, 90.0).await;
    mode.input(AutonomyModeInput::Activate).await;
    timeout(Duration::from_secs(2), record(directory.path()))
        .await
        .expect("shutdown must not wait for EDS");
    timeout(Duration::from_secs(2), async {
        while Path::new(&format!("/proc/{pid}")).exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cancelled EDS process must exit");
    std::mem::forget(cleanup.take().unwrap()); // PID is reaped; do not signal a potentially reused PID.
    mode.silent_for(Duration::from_millis(100)).await;
    mode.stop().await;
}
