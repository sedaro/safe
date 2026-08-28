use std::path::Path;
use std::process::{Child, Command};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::Value;

const COMMAND_ID: &str = "1:00000000-0000-0000-0000-000000000001:0";
const SECOND_COMMAND_ID: &str = "2:00000000-0000-0000-0000-000000000001:0";

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
fn external_egress_acknowledgement_marks_seeded_commands_published() {
    let tempdir = tempfile::tempdir().unwrap();
    let mut safe = start_safe_with_egress(&tempdir, false);

    let board = wait_for_board_entry(
        &tempdir.path().join("state/status.json"),
        COMMAND_ID,
        "published",
    );
    assert_eq!(board["id"], COMMAND_ID);
    assert!(safe.0.try_wait().unwrap().is_none());
}

#[test]
fn external_egress_clear_request_cancels_only_the_requested_command() {
    let tempdir = tempfile::tempdir().unwrap();
    let mut safe = start_safe_with_egress(&tempdir, true);

    let board = wait_for_board_entry(
        &tempdir.path().join("state/status.json"),
        COMMAND_ID,
        "rejected",
    );
    assert_eq!(board["id"], COMMAND_ID);
    assert_eq!(board["decision_by"], "ffffffff-ffff-ffff-ffff-ffffffffffff");
    assert_eq!(board["decision_reason"], "host schedule cleared");
    wait_for_board_entry(
        &tempdir.path().join("state/status.json"),
        SECOND_COMMAND_ID,
        "published",
    );
    assert!(safe.0.try_wait().unwrap().is_none());
}

#[test]
fn repeated_external_clear_requests_are_idempotent() {
    let tempdir = tempfile::tempdir().unwrap();
    let mut safe = start_safe_with_egress(&tempdir, true);

    let outputs = tempdir.path().join("state/outputs.jsonl");
    wait_for_rejection_count(&outputs, COMMAND_ID, 1);
    sleep(Duration::from_millis(300));
    let rejection_count = wait_for_rejection_count(&outputs, COMMAND_ID, 1);

    assert_eq!(
        rejection_count, 1,
        "the same clear request was applied twice"
    );
    assert!(safe.0.try_wait().unwrap().is_none());
}

#[test]
fn compacted_output_journal_restores_board_after_restart() {
    let tempdir = tempfile::tempdir().unwrap();
    let mut safe = start_safe_with_egress(&tempdir, true);

    wait_for_board_entry(
        &tempdir.path().join("state/status.json"),
        COMMAND_ID,
        "rejected",
    );
    wait_for_output_snapshot(&tempdir.path().join("state/outputs.jsonl"));
    safe.stop();

    let mut restarted = start_safe_with_egress(&tempdir, true);
    let board = wait_for_board_entry(
        &tempdir.path().join("state/status.json"),
        COMMAND_ID,
        "rejected",
    );

    assert_eq!(board["decision_reason"], "host schedule cleared");
    assert!(restarted.0.try_wait().unwrap().is_none());
}

fn start_safe_with_egress(tempdir: &tempfile::TempDir, clear_board: bool) -> SafeProcess {
    let base = tempdir.path();
    let state = base.join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(base.join("modes.json"), "[]").unwrap();
    let outputs = state.join("outputs.jsonl");
    if !outputs.exists() {
        seed_approved_board(&outputs);
    }

    let response = if clear_board {
        r#"printf '%s\n' '{"kind":"board_published","command_ids":["1:00000000-0000-0000-0000-000000000001:0","2:00000000-0000-0000-0000-000000000001:0"]}' '{"kind":"clear_board_commands","command_ids":["1:00000000-0000-0000-0000-000000000001:0"],"reason":"host schedule cleared"}'"#
    } else {
        r#"printf '%s\n' '{"kind":"board_published","command_ids":["1:00000000-0000-0000-0000-000000000001:0","2:00000000-0000-0000-0000-000000000001:0"]}'"#
    };
    let script_path = base.join("egress.sh");
    std::fs::write(
        &script_path,
        format!(
            "#!/usr/bin/env bash\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *board_snapshot*) {response} ;;\n  esac\ndone\n"
        ),
    )
    .unwrap();

    let config_path = base.join("safe.yaml");
    std::fs::write(
        &config_path,
        format!(
            "base_paths:\n  base_working_directory: {base}\n  base_writable_directory: {base}\nlogging:\n  file_path: {base}/logs/safe.log\npersistence:\n  events_max_bytes: 1048576\n  events_max_records: 1000\n  outputs_max_bytes: 1048576\n  outputs_max_records: 1\nplatform:\n  telemetry_adapter: example\n  command_adapter: safectl_unix_json\n  egress_adapter: external\n  external_egress_command: \"bash {script}\"\n  gatekeeper_adapter: disabled\n",
            base = base.display(),
            script = script_path.display(),
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

fn seed_approved_board(path: &Path) {
    let events = [
        serde_json::json!({
            "Board": {
                "Proposed": {
                    "id": COMMAND_ID,
                    "from": "00000000-0000-0000-0000-000000000001",
                    "cmd": { "Scheduled": { "cmd": "PointNadir", "gps_time": 70.0 } },
                    "ts_mono": 70
                }
            }
        }),
        serde_json::json!({
            "Board": {
                "Approved": {
                    "id": COMMAND_ID,
                    "by": "00000000-0000-0000-0000-000000000000",
                    "reason": "test approval",
                    "ts_mono": 71
                }
            }
        }),
        serde_json::json!({
            "Board": {
                "Proposed": {
                    "id": SECOND_COMMAND_ID,
                    "from": "00000000-0000-0000-0000-000000000001",
                    "cmd": { "Scheduled": { "cmd": "PointSunYaw", "gps_time": 80.0 } },
                    "ts_mono": 80
                }
            }
        }),
        serde_json::json!({
            "Board": {
                "Approved": {
                    "id": SECOND_COMMAND_ID,
                    "by": "00000000-0000-0000-0000-000000000000",
                    "reason": "test approval",
                    "ts_mono": 81
                }
            }
        }),
    ];
    let contents = events
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, format!("{contents}\n")).unwrap();
}

fn wait_for_board_entry(status_path: &Path, command_id: &str, expected_state: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(contents) = std::fs::read_to_string(status_path)
            && let Ok(status) = serde_json::from_str::<Value>(&contents)
            && let Some(board) = status["board"].as_array()
            && let Some(entry) = board
                .iter()
                .find(|entry| entry["id"] == command_id && entry["state"] == expected_state)
        {
            return entry.clone();
        }
        sleep(Duration::from_millis(25));
    }

    panic!(
        "timed out waiting for {expected_state} board entry at {}",
        status_path.display()
    );
}

fn wait_for_output_snapshot(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(contents) = std::fs::read_to_string(path)
            && let Ok(record) = serde_json::from_str::<Value>(contents.trim())
            && record.get("Snapshot").is_some()
        {
            return;
        }
        sleep(Duration::from_millis(25));
    }

    panic!(
        "timed out waiting for output journal snapshot at {}",
        path.display()
    );
}

fn wait_for_rejection_count(path: &Path, command_id: &str, minimum: usize) -> usize {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(contents) = std::fs::read_to_string(path)
            && let Ok(record) = serde_json::from_str::<Value>(contents.trim())
            && let Some(rejections) = record["Snapshot"]["board"]["rejected"][command_id].as_array()
            && rejections.len() >= minimum
        {
            return rejections.len();
        }
        sleep(Duration::from_millis(25));
    }

    panic!(
        "timed out waiting for {minimum} rejections for {command_id} at {}",
        path.display()
    );
}
