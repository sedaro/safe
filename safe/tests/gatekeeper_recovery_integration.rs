use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct SafeProcess(Child);

impl Drop for SafeProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const MODE: &str = "00000000-0000-0000-0000-000000000001";

fn start(base: &Path, gatekeeper: &str, timeout_ms: u64) -> SafeProcess {
    let modes = base.join("modes.json");
    std::fs::write(&modes, "[]").unwrap();
    std::fs::write(base.join("gatekeeper.sh"), gatekeeper).unwrap();
    std::fs::write(
        base.join("safe.yaml"),
        format!(
            "base_paths:\n  base_writable_directory: {}\nlogging:\n  file_path: {}/logs/safe.log\nplatform:\n  telemetry_adapter: external\n  external_telemetry_command: 'true'\n  egress_adapter: safectl_filesystem\n  gatekeeper_adapter: external\n  external_gatekeeper_command: '. {}/gatekeeper.sh'\n  external_gatekeeper_request_timeout_ms: {timeout_ms}\n",
            base.display(), base.display(), base.display()
        ),
    )
    .unwrap();
    let log = std::fs::File::create(base.join("safe-console.log")).unwrap();
    let stderr = log.try_clone().unwrap();
    SafeProcess(
        Command::new(env!("CARGO_BIN_EXE_safe"))
            .env("SAFE_RUNTIME_CONFIG", base.join("safe.yaml"))
            .env("SAFE_AUTONOMY_MODE_CONFIG_PATH", modes)
            .stdout(std::process::Stdio::from(log))
            .stderr(std::process::Stdio::from(stderr))
            .spawn()
            .unwrap(),
    )
}

fn seed_proposal(base: &Path, command: Value, cached_as_accepted: bool) -> String {
    let state = base.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let id = format!("42:{MODE}:0");
    let snapshot = json!({"Snapshot":{"board":{
        "proposals":{&id:[MODE,command,17741]},"approved":{},"rejected":{},
        "source_of_truth":if cached_as_accepted {vec![id.clone()]} else {vec![]}
    }}});
    std::fs::write(state.join("outputs.jsonl"), format!("{snapshot}\n")).unwrap();
    id
}

fn wait_for(base: &Path, check: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(base.join("state/status.json"))
            && let Ok(status) = serde_json::from_str::<Value>(&text)
            && check(&status)
        {
            return status;
        }
        sleep(Duration::from_millis(20));
    }
    panic!(
        "condition not reached; status={} console={}",
        std::fs::read_to_string(base.join("state/status.json")).unwrap_or_default(),
        std::fs::read_to_string(base.join("safe-console.log")).unwrap_or_default()
    );
}

fn send_telemetry(base: &Path) {
    wait_for(base, |_| base.join("state/safectl.sock").exists());
    let mut socket = UnixStream::connect(base.join("state/safectl.sock")).unwrap();
    writeln!(
        socket,
        "{}",
        json!({"type":"telemetry","telemetry":{
            "source":"recovery-test","ts_mono":1,"payload":"{}"
        }})
    )
    .unwrap();
}

#[test]
fn restored_noops_keep_gatekeeper_validation_and_recover_without_a_state_reset() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    let first = format!("42:{MODE}:0");
    let second_mode = "00000000-0000-0000-0000-000000000002";
    let second_id = format!("43:{second_mode}:0");
    std::fs::create_dir_all(base.join("state")).unwrap();
    let snapshot = json!({"Snapshot":{"board":{
        "proposals":{
            &first:[MODE,"NOOP",17741],
            &second_id:[second_mode,"NOOP",17742]
        },"approved":{},"rejected":{},"source_of_truth":[first,second_id]
    }}});
    std::fs::write(base.join("state/outputs.jsonl"), format!("{snapshot}\n")).unwrap();
    let script = format!(
        r#"while IFS= read -r line; do
case "$line" in *'"kind":"evaluate_batch"'*)
 printf '%s\n' "$line" >> '{base}/requests.jsonl'
 id="${{line#*\"request_id\":}}"; id="${{id%%,*}}"
 printf '{{"kind":"approve","request_id":%s,"details":"fixture","simulation_count":1}}\n' "$id";;
esac
done
"#,
        base = base.display()
    );
    let mut safe = start(base, &script, 100);
    send_telemetry(base);
    let status = wait_for(base, |s| {
        s["board"].as_array().is_some_and(|rows| {
            rows.len() == 2
                && rows
                    .iter()
                    .all(|r| r["decision_reason"] == "Gatekeeper approved command")
        })
    });
    assert!(
        status["board"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["decision_by"] == "gatekeeper")
    );
    let requests: Vec<Value> = std::fs::read_to_string(base.join("requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]["candidate_command_ids"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    safe.0.kill().unwrap();
    safe.0.wait().unwrap();
    let restarted = start(base, "exit 23\n", 100);
    wait_for(base, |s| {
        s["daemon"]["pid"] == restarted.0.id()
            && s["board"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["decision_by"] == "gatekeeper")
                .count()
                == 2
    });
}

#[test]
fn physical_proposal_retries_after_gatekeeper_exit_without_approving_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    let id = seed_proposal(
        base,
        json!({"Scheduled":{"cmd":"PointNadir","gps_time":1475891700.0}}),
        false,
    );
    let script = format!(
        r#"
if [ ! -f '{base}/first' ]; then touch '{base}/first'; exit 23; fi
while IFS= read -r line; do
 case "$line" in *'"kind":"evaluate_batch"'*)
   id="${{line#*\"request_id\":}}"; id="${{id%%,*}}"
    printf '{{"kind":"approve","request_id":%s,"details":"fixture","simulation_count":1}}\n' "$id";;
  esac
done
"#,
        base = base.display()
    );
    let _safe = start(base, &script, 100);
    send_telemetry(base);
    let status = wait_for(base, |s| {
        base.join("first").exists()
            && s["board"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == id && r["state"] == "pending")
    });
    assert!(
        status["board"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == id && r["decision_by"].is_null())
    );
    wait_for(base, |s| {
        s["board"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == id && r["decision_reason"] == "Gatekeeper approved command")
    });
}

#[test]
fn silent_gatekeeper_is_bounded_and_pending_physical_work_is_retried() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    let id = seed_proposal(
        base,
        json!({"Scheduled":{"cmd":"PointNadir","gps_time":1475891700.0}}),
        false,
    );
    let script = format!(
        r#"
if [ ! -f '{base}/first' ]; then touch '{base}/first'; while IFS= read -r line; do :; done; fi
while IFS= read -r line; do
 case "$line" in *'"kind":"evaluate_batch"'*)
   id="${{line#*\"request_id\":}}"; id="${{id%%,*}}"
    printf '{{"kind":"approve","request_id":%s,"details":"fixture","simulation_count":1}}\n' "$id";;
  esac
done
"#,
        base = base.display()
    );
    let _safe = start(base, &script, 100);
    send_telemetry(base);
    let status = wait_for(base, |s| {
        base.join("first").exists()
            && s["board"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == id && r["state"] == "pending")
    });
    assert!(
        status["board"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == id && r["decision_by"].is_null())
    );
    wait_for(base, |s| {
        s["board"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == id && r["decision_reason"] == "Gatekeeper approved command")
    });
}

#[test]
fn recovery_repairs_a_board_effect_missing_after_the_checkpoint_was_written() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    std::fs::create_dir_all(base.join("state")).unwrap();
    std::fs::write(base.join("modes.json"), "[]").unwrap();
    std::fs::write(base.join("state/outputs.jsonl"), "").unwrap();
    let id = format!("10:{MODE}:0");
    let event = json!({"seq":10,"ts_mono":10,"source":"AutonomyMode",
        "msg":{"AutonomyModeCommandReceived":{"from":MODE,"cmd":"NOOP"}}});
    std::fs::write(base.join("state/events.jsonl"), format!("{event}\n")).unwrap();
    std::fs::write(
        base.join("state/flight.json"),
        json!({
            "running":true,"halted":false,"fault":null,"last_seq_applied":10,
            "active_autonomy_mode":null,"manual_active_override":null,"autonomy_modes":[],
            "autonomy_mode_activations":[],"last_planned_autonomy_mode":null
        })
        .to_string(),
    )
    .unwrap();
    let script = r#"while IFS= read -r line; do
case "$line" in *'"kind":"evaluate_batch"'*)
 id="${line#*\"request_id\":}"; id="${id%%,*}"
 printf '{"kind":"approve","request_id":%s,"details":"fixture","simulation_count":1}\n' "$id";;
esac
done"#;
    let _safe = start(base, script, 100);
    send_telemetry(base);
    wait_for(base, |s| {
        s["board"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == id && r["decision_by"] == "gatekeeper")
    });
    let output = std::fs::read_to_string(base.join("state/outputs.jsonl")).unwrap();
    assert!(output.contains("Proposed"));
    assert!(output.contains("Approved"));
}

#[test]
fn restored_accepted_cache_is_recomputed_before_pending_work_is_selected() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    let id = seed_proposal(
        base,
        json!({"Scheduled":{"cmd":"PointNadir","gps_time":1475891700.0}}),
        true,
    );
    let script = r#"while IFS= read -r line; do
case "$line" in *'"kind":"evaluate_batch"'*)
id="${line#*\"request_id\":}"; id="${id%%,*}"
printf '{"kind":"approve","request_id":%s,"details":"fixture","simulation_count":1}\n' "$id";;
esac
done"#;
    let _safe = start(base, script, 100);
    send_telemetry(base);
    wait_for(base, |s| {
        s["board"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == id && r["decision_reason"] == "Gatekeeper approved command")
    });
}
