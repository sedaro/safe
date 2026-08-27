use std::io::{BufRead, Write};
use std::process::{Command as ProcessCommand, Stdio};

use safe::protocol::{AutonomyModeId, BoardCmdId, BoardState, Command, TimedCommand};
use safe::runtime::{HostCommandStatus, HostCommandStatusState};

#[test]
fn example_egress_writes_filesystem_outputs_and_acknowledges_the_board() {
    let tempdir = tempfile::tempdir().unwrap();
    let mut board = BoardState::default();
    let id = BoardCmdId("1:00000000-0000-0000-0000-000000000001:0".to_string());
    board.proposals.insert(
        id.clone(),
        (
            AutonomyModeId(uuid::Uuid::from_u128(1)),
            TimedCommand::Scheduled {
                cmd: Command::PointNadir,
                gps_time: 70.0,
            },
            70,
        ),
    );
    board.source_of_truth = vec![id.clone()];

    let mut child = ProcessCommand::new(env!("CARGO_BIN_EXE_platform-egress-example"))
        .arg("--base-path")
        .arg(tempdir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({ "kind": "board_snapshot", "board": board })
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "kind": "host_command_status",
            "status": HostCommandStatus {
                request_id: "request-1".to_string(),
                state: HostCommandStatusState::Accepted,
                detail: "command accepted".to_string(),
                ts_mono: 42,
            },
        })
    )
    .unwrap();
    drop(stdin);

    let mut output = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut output)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(output.trim()).unwrap(),
        serde_json::json!({ "kind": "board_published", "command_ids": [id] })
    );
    assert!(child.wait().unwrap().success());

    let csv = std::fs::read_to_string(tempdir.path().join("out/commands.csv")).unwrap();
    assert_eq!(csv, "cmd,gps_time\nPointNadir,70.0\n");
    let status =
        std::fs::read_to_string(tempdir.path().join("state/host_command_status.jsonl")).unwrap();
    assert!(status.contains("\"request_id\":\"request-1\""));
}
