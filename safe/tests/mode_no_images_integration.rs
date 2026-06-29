use std::time::Duration;

use safe::protocol::{
    AutonomyModeBoardState, AutonomyModeId, AutonomyModeInput, BoardCmdId, ModeToSafe, SafeToMode,
};
use safe::transports::Transport;
use safe::transports::unix::UnixTransport;
use tempfile::tempdir;
use tokio::process::Command;
use tokio::time::timeout;
use uuid::Uuid;

#[tokio::test]
async fn no_images_mode_handshake_and_cancel_flow() {
    let mode_id = AutonomyModeId(Uuid::from_u128(1));
    let td = tempdir().unwrap();
    let socket_path = td.path().join("mode_no_images.sock");
    let config_path = td.path().join("mode_config.json");
    tokio::fs::write(&config_path, r#"{"cancel_reason":"No imaging allowed"}"#)
        .await
        .unwrap();

    let mut server =
        UnixTransport::<ModeToSafe, SafeToMode>::new(socket_path.to_string_lossy().as_ref())
            .await
            .unwrap();

    let Some(mode_bin) = std::env::var_os("CARGO_BIN_EXE_mode_stationkeeping") else {
        return;
    };

    let mut child = Command::new(mode_bin)
        .arg("--endpoint")
        .arg(socket_path.to_string_lossy().to_string())
        .arg("--config")
        .arg(config_path.to_string_lossy().to_string())
        .arg("--mode-id")
        .arg(mode_id.to_string())
        .spawn()
        .unwrap();

    let mut stream = timeout(Duration::from_secs(5), server.accept())
        .await
        .expect("accept timeout")
        .expect("accept failed");

    stream
        .write(SafeToMode::Hello {
            expected_mode: mode_id,
        })
        .await
        .unwrap();

    let hello = timeout(Duration::from_secs(5), stream.read())
        .await
        .expect("hello timeout")
        .expect("hello read failed");
    match hello {
        ModeToSafe::Hello {
            mode,
            protocol_version,
        } => {
            assert_eq!(mode, mode_id);
            assert_eq!(protocol_version, 1);
        }
        _ => panic!("expected hello"),
    }

    stream
        .write(SafeToMode::Input(AutonomyModeInput::Activate))
        .await
        .unwrap();

    let board = AutonomyModeBoardState {
        source_of_truth: vec![BoardCmdId(format!("abc:{mode_id}:0"))],
        ..Default::default()
    };
    stream
        .write(SafeToMode::Input(AutonomyModeInput::BoardSnapshot(board)))
        .await
        .unwrap();

    let cancel = timeout(Duration::from_secs(5), stream.read())
        .await
        .expect("cancel timeout")
        .expect("cancel read failed");
    match cancel {
        ModeToSafe::Output(safe::protocol::AutonomyModeOutput::CancelBoard { id, reason }) => {
            assert_eq!(id.0, format!("abc:{mode_id}:0"));
            assert_eq!(reason, "No imaging allowed");
        }
        _ => panic!("expected cancel output"),
    }

    stream
        .write(SafeToMode::Input(AutonomyModeInput::Shutdown))
        .await
        .unwrap();

    let _ = timeout(Duration::from_secs(5), child.wait()).await;
    let _ = child.kill().await;
}
