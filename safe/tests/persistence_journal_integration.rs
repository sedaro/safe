use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::Value;

struct SafeProcess(Child);

impl Drop for SafeProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl SafeProcess {
    fn stop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn event_journal_compacts_after_durable_checkpoint_and_restart() {
    let tempdir = tempfile::tempdir().unwrap();
    let mut safe = start_safe(&tempdir);
    let state = tempdir.path().join("state");
    let socket = state.join("safectl.sock");

    wait_for_path(&socket);
    send_telemetry(&socket);
    wait_for_applied_event(&state.join("flight.json"));
    wait_for_telemetry_source(&state.join("status.json"), "integration-test");
    wait_for_empty_file(&state.join("events.jsonl"));
    safe.stop();

    let mut restarted = start_safe(&tempdir);
    wait_for_path(&state.join("status.json"));

    assert!(restarted.0.try_wait().unwrap().is_none());
    assert!(
        std::fs::read_to_string(state.join("events.jsonl"))
            .unwrap_or_default()
            .is_empty()
    );
}

fn start_safe(tempdir: &tempfile::TempDir) -> SafeProcess {
    let base = tempdir.path();
    let config_path = base.join("safe.yaml");
    std::fs::write(base.join("modes.json"), "[]").unwrap();
    std::fs::write(
        &config_path,
        format!(
            "base_paths:\n  base_working_directory: {base}\n  base_writable_directory: {base}\nlogging:\n  file_path: {base}/logs/safe.log\npersistence:\n  events_max_bytes: 1048576\n  events_max_records: 1\n  outputs_max_bytes: 1048576\n  outputs_max_records: 1000\nplatform:\n  telemetry_adapter: example\n  command_adapter: safectl_unix_json\n  egress_adapter: safectl_filesystem\n  gatekeeper_adapter: disabled\n",
            base = base.display(),
        ),
    )
    .unwrap();

    SafeProcess(
        Command::new(env!("CARGO_BIN_EXE_safe"))
            .env("SAFE_RUNTIME_CONFIG", config_path)
            .env("SAFE_AUTONOMY_MODE_CONFIG_PATH", base.join("modes.json"))
            .env("SAFE_SANDBOX_ISOLATION", "disabled")
            .spawn()
            .unwrap(),
    )
}

fn send_telemetry(socket: &Path) {
    let mut stream = UnixStream::connect(socket).unwrap();
    let ingress = serde_json::json!({
        "type": "telemetry",
        "telemetry": {
            "source": "integration-test",
            "ts_mono": 1,
            "payload": "{\"temperature\":42}"
        }
    });
    writeln!(stream, "{ingress}").unwrap();
}

fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {}", path.display());
}

fn wait_for_applied_event(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(contents) = std::fs::read_to_string(path)
            && let Ok(flight) = serde_json::from_str::<Value>(&contents)
            && flight["last_seq_applied"]
                .as_u64()
                .is_some_and(|seq| seq > 0)
        {
            return;
        }
        sleep(Duration::from_millis(25));
    }
    panic!(
        "timed out waiting for durable flight checkpoint at {}",
        path.display()
    );
}

fn wait_for_empty_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if std::fs::read_to_string(path).is_ok_and(|contents| contents.is_empty()) {
            return;
        }
        sleep(Duration::from_millis(25));
    }
    panic!(
        "timed out waiting for compacted journal at {}",
        path.display()
    );
}

fn wait_for_telemetry_source(path: &Path, expected_source: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(contents) = std::fs::read_to_string(path)
            && let Ok(status) = serde_json::from_str::<Value>(&contents)
            && status["telemetry"]["latest"]["source"] == expected_source
        {
            return;
        }
        sleep(Duration::from_millis(25));
    }
    panic!(
        "timed out waiting for telemetry source {expected_source} at {}",
        path.display()
    );
}
