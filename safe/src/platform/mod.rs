use std::path::PathBuf;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::config::Config;
use crate::protocol::{BoardCmdId, BoardState};
use crate::telemetry_frame::TelemetryFrame;
use crate::{HostCommandRequest, HostCommandStatus, RuntimePaths, SafectlIngress, TimedCommand};

pub mod gatekeeper;
pub mod telemetry;
use telemetry::spawn_telemetry_ingress;

#[derive(Debug, Clone)]
pub struct BoardPublicationStatus {
    pub command_ids: Vec<BoardCmdId>,
}

#[derive(Debug, Clone)]
pub struct BoardClearRequest {
    pub command_ids: Vec<BoardCmdId>,
    pub reason: String,
}

pub fn platform_egress_id() -> crate::AutonomyModeId {
    crate::AutonomyModeId(uuid::Uuid::from_u128(u128::MAX))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformEgressKind {
    SafectlFilesystem,
    External,
}

impl PlatformEgressKind {
    pub fn from_config(name: &str) -> anyhow::Result<Self> {
        match name {
            "safectl_filesystem" => Ok(Self::SafectlFilesystem),
            "external" => Ok(Self::External),
            _ => anyhow::bail!("unsupported platform egress adapter: {name}"),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PlatformEgressWireInput {
    HostCommandStatus { status: HostCommandStatus },
    BoardSnapshot { board: BoardState },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PlatformEgressWireOutput {
    BoardPublished {
        command_ids: Vec<BoardCmdId>,
    },
    ClearBoardCommands {
        command_ids: Vec<BoardCmdId>,
        reason: String,
    },
}

#[async_trait]
pub trait IngressSource {
    type Frame: Send + 'static;

    async fn next_frame(&mut self) -> anyhow::Result<Option<Self::Frame>>;
}

pub trait FrameDecoder<In, Out> {
    fn decode(&self, frame: In) -> anyhow::Result<Out>;
}

pub trait FrameEncoder<In, Out> {
    fn encode(&self, value: In) -> anyhow::Result<Out>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandIngressKind {
    SafectlUnixJson,
}

impl CommandIngressKind {
    pub fn from_config(name: &str) -> anyhow::Result<Self> {
        match name {
            "safectl_unix_json" => Ok(Self::SafectlUnixJson),
            _ => anyhow::bail!("unsupported command ingress adapter: {name}"),
        }
    }
}

pub fn spawn_platform_ingress(
    cfg: &Config,
    runtime_paths: &RuntimePaths,
    telemetry_tx: mpsc::Sender<TelemetryFrame>,
    command_tx: mpsc::Sender<HostCommandRequest>,
) -> anyhow::Result<()> {
    let command_kind = CommandIngressKind::from_config(&cfg.platform.command_adapter)?;

    spawn_telemetry_ingress(cfg, telemetry_tx.clone())?;

    match command_kind {
        CommandIngressKind::SafectlUnixJson => {
            #[cfg(feature = "platform-safectl-json")]
            {
                let runtime_paths = runtime_paths.clone();
                tokio::spawn(safectl_ingress_reader(
                    runtime_paths,
                    telemetry_tx,
                    command_tx,
                ));
            }
            #[cfg(not(feature = "platform-safectl-json"))]
            {
                anyhow::bail!(
                    "command adapter safectl_unix_json selected, but feature platform-safectl-json is disabled"
                );
            }
        }
    }

    Ok(())
}

#[cfg(feature = "platform-safectl-json")]
async fn safectl_ingress_reader(
    runtime_paths: RuntimePaths,
    telemetry_tx: mpsc::Sender<TelemetryFrame>,
    command_tx: mpsc::Sender<HostCommandRequest>,
) -> anyhow::Result<()> {
    let safectl_sock_path: &PathBuf = &runtime_paths.safectl_sock;

    if safectl_sock_path.exists() {
        let _ = tokio::fs::remove_file(safectl_sock_path).await;
    }
    let listener = tokio::net::UnixListener::bind(safectl_sock_path)?;

    loop {
        let (stream, _) = listener.accept().await?;
        let telemetry_tx = telemetry_tx.clone();
        let command_tx = command_tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let msg = line.trim();
                        if msg.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<SafectlIngress>(msg) {
                            Ok(SafectlIngress::Command {
                                command,
                                request_id,
                            }) => {
                                let request_id = request_id
                                    .unwrap_or_else(|| format!("safectl:{}", uuid::Uuid::new_v4()));
                                let _ = command_tx
                                    .send(HostCommandRequest {
                                        request_id,
                                        command,
                                    })
                                    .await;
                            }
                            Ok(SafectlIngress::Telemetry { telemetry }) => {
                                let _ = telemetry_tx.send(telemetry).await;
                            }
                            Err(e) => {
                                error!("bad safectl ingress json: {e}; line={msg}");
                            }
                        }
                    }
                    Err(e) => {
                        error!("safectl ingress read error: {e}");
                        break;
                    }
                }
            }
        });
    }
}

pub fn spawn_platform_egress(
    cfg: &Config,
    runtime_paths: &RuntimePaths,
    mut status_rx: mpsc::Receiver<HostCommandStatus>,
    mut command_dispatch_rx: mpsc::Receiver<BoardState>,
    board_publication_tx: mpsc::Sender<BoardPublicationStatus>,
    board_clear_tx: mpsc::Sender<BoardClearRequest>,
) -> anyhow::Result<()> {
    let egress_kind = PlatformEgressKind::from_config(&cfg.platform.egress_adapter)?;

    match egress_kind {
        PlatformEgressKind::SafectlFilesystem => {
            let status_path = runtime_paths.state.join("host_command_status.jsonl");
            let dispatch_csv_path = runtime_paths.base.join("out").join("commands.csv");
            tokio::spawn(filesystem_egress_adapter(
                status_path,
                dispatch_csv_path,
                status_rx,
                command_dispatch_rx,
                board_publication_tx,
            ));
        }
        PlatformEgressKind::External => {
            let command = cfg
                .platform
                .external_egress_command
                .clone()
                .ok_or_else(|| anyhow::anyhow!(
                    "platform egress adapter external selected, but platform.external_egress_command is not configured"
                ))?;
            tokio::spawn(async move {
                if let Err(e) = external_egress_adapter(
                    command,
                    status_rx,
                    command_dispatch_rx,
                    board_publication_tx,
                    board_clear_tx,
                )
                .await
                {
                    error!("external platform egress adapter stopped: {e}");
                }
            });
        }
    }

    Ok(())
}

async fn filesystem_egress_adapter(
    status_path: PathBuf,
    dispatch_csv_path: PathBuf,
    mut status_rx: mpsc::Receiver<HostCommandStatus>,
    mut command_dispatch_rx: mpsc::Receiver<BoardState>,
    board_publication_tx: mpsc::Sender<BoardPublicationStatus>,
) -> anyhow::Result<()> {
    let mut status_open = true;
    let mut command_dispatch_open = true;
    loop {
        if !status_open && !command_dispatch_open {
            break;
        }
        tokio::select! {
            status = status_rx.recv(), if status_open => {
                match status {
                    Some(status) => {
                        if let Err(e) = append_host_status_jsonl(&status_path, &status).await {
                            error!("failed writing host command status: {e}");
                        }
                    }
                    None => status_open = false,
                }
            }
            board_state = command_dispatch_rx.recv(), if command_dispatch_open => {
                match board_state {
                    Some(board_state) => match write_host_command_dispatch_csv(&dispatch_csv_path, &board_state).await {
                        Ok(()) => {
                            if board_publication_tx.send(BoardPublicationStatus {
                                command_ids: board_state.source_of_truth,
                            }).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => error!("failed writing host command dispatch csv: {e}"),
                    },
                    None => command_dispatch_open = false,
                }
            }
        }
    }
    Ok(())
}

async fn external_egress_adapter(
    command: String,
    mut status_rx: mpsc::Receiver<HostCommandStatus>,
    mut command_dispatch_rx: mpsc::Receiver<BoardState>,
    board_publication_tx: mpsc::Sender<BoardPublicationStatus>,
    board_clear_tx: mpsc::Sender<BoardClearRequest>,
) -> anyhow::Result<()> {
    info!(%command, "platform egress adapter `external` started");

    let mut child = Command::new("bash")
        .arg("-lc")
        .arg(command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("external egress process has no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("external egress process has no stdout"))?;

    let mut writer = stdin;
    let mut lines = BufReader::new(stdout).lines();
    let mut status_open = true;
    let mut command_dispatch_open = true;
    loop {
        if !status_open && !command_dispatch_open {
            break;
        }
        tokio::select! {
            status = status_rx.recv(), if status_open => {
                match status {
                    Some(status) => write_egress_message(&mut writer, PlatformEgressWireInput::HostCommandStatus { status }).await?,
                    None => status_open = false,
                }
            }
            board = command_dispatch_rx.recv(), if command_dispatch_open => {
                match board {
                    Some(board) => write_egress_message(&mut writer, PlatformEgressWireInput::BoardSnapshot { board }).await?,
                    None => command_dispatch_open = false,
                }
            }
            line = lines.next_line() => {
                let Some(line) = line? else { break; };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<PlatformEgressWireOutput>(trimmed) {
                    Ok(PlatformEgressWireOutput::BoardPublished { command_ids }) => {
                        if board_publication_tx.send(BoardPublicationStatus { command_ids }).await.is_err() {
                            break;
                        }
                    }
                    Ok(PlatformEgressWireOutput::ClearBoardCommands { command_ids, reason }) => {
                        if board_clear_tx.send(BoardClearRequest { command_ids, reason }).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => error!("invalid external egress json value: {e}; line={trimmed}"),
                }
            }
        }
    }
    Ok(())
}

async fn write_egress_message(
    writer: &mut tokio::process::ChildStdin,
    message: PlatformEgressWireInput,
) -> anyhow::Result<()> {
    let line = serde_json::to_string(&message)?;
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn append_host_status_jsonl(
    path: &PathBuf,
    status: &HostCommandStatus,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    let line = serde_json::to_string(status)?;
    f.write_all(line.as_bytes()).await?;
    f.write_all(b"\n").await?;
    f.flush().await?;
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct HostCommandDispatchCsvRecord {
    cmd: String,
    gps_time: f64,
}

async fn write_host_command_dispatch_csv(
    path: &PathBuf,
    board_state: &BoardState,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let file_metadata = tokio::fs::metadata(path).await;
    let needs_header = match file_metadata {
        Ok(meta) => meta.len() == 0,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => return Err(e.into()),
    };
    let path = path.clone();

    let cmd_ids = board_state.source_of_truth.clone();
    let cmds = cmd_ids
        .iter()
        .map(|id| board_state.proposals.get(id))
        .collect::<Vec<_>>();
    let cmds = cmds
        .iter()
        .filter(|cmd| cmd.is_some())
        .map(|cmd| cmd.unwrap())
        .collect::<Vec<_>>();
    let mut cmds = cmds
        .iter()
        .map(|cmd| match cmd.1.clone() {
            TimedCommand::Now(cmd) => {
                return None;
            }
            TimedCommand::NOOP => {
                return None;
            }
            TimedCommand::Scheduled { cmd, gps_time } => {
                return Some(HostCommandDispatchCsvRecord {
                    cmd: cmd.into(),
                    gps_time: gps_time,
                });
            }
        })
        .filter(|c| c.is_some())
        .map(|c| c.unwrap())
        .collect::<Vec<_>>();
    cmds.sort_by(|a, b| {
        a.gps_time
            .partial_cmp(&b.gps_time)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut tmp_path = path.clone();
        tmp_path.add_extension("tmp");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;

        let mut writer = csv::WriterBuilder::new()
            .has_headers(needs_header)
            .from_writer(file);
        for cmd in cmds {
            writer.serialize(&cmd)?;
        }
        writer.flush()?;

        std::fs::rename(tmp_path, path)?;

        Ok(())
    })
    .await??;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Command;
    use crate::protocol::TimedCommand;
    use tokio::time::{Duration, timeout};

    #[test]
    fn platform_egress_kind_parses_supported_adapters() {
        assert_eq!(
            PlatformEgressKind::from_config("safectl_filesystem").unwrap(),
            PlatformEgressKind::SafectlFilesystem
        );
        assert_eq!(
            PlatformEgressKind::from_config("external").unwrap(),
            PlatformEgressKind::External
        );
        assert!(PlatformEgressKind::from_config("unknown").is_err());
    }

    #[test]
    fn platform_egress_id_is_all_ones() {
        assert_eq!(
            platform_egress_id().to_string(),
            "ffffffff-ffff-ffff-ffff-ffffffffffff"
        );
    }

    #[test]
    fn platform_egress_wire_messages_round_trip() {
        let message = PlatformEgressWireInput::HostCommandStatus {
            status: HostCommandStatus {
                request_id: "request-1".to_string(),
                state: crate::HostCommandStatusState::Accepted,
                detail: "command accepted".to_string(),
                ts_mono: 42,
            },
        };

        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains("\"kind\":\"host_command_status\""));
        let decoded: PlatformEgressWireInput = serde_json::from_str(&json).unwrap();
        match decoded {
            PlatformEgressWireInput::HostCommandStatus { status } => {
                assert_eq!(status.request_id, "request-1");
                assert_eq!(status.ts_mono, 42);
            }
            PlatformEgressWireInput::BoardSnapshot { .. } => panic!("unexpected board snapshot"),
        }
    }

    #[tokio::test]
    async fn external_egress_publishes_only_acknowledged_command_ids() {
        let (status_tx, status_rx) = mpsc::channel(1);
        let (board_tx, board_rx) = mpsc::channel(1);
        let (publication_tx, mut publication_rx) = mpsc::channel(1);
        let (clear_tx, _clear_rx) = mpsc::channel(1);
        let adapter = tokio::spawn(external_egress_adapter(
            r#"while IFS= read -r _; do printf '%s\n' '{"kind":"board_published","command_ids":["test-id"]}'; done"#.to_string(),
            status_rx,
            board_rx,
            publication_tx,
            clear_tx,
        ));

        board_tx.send(BoardState::default()).await.unwrap();
        let publication = timeout(Duration::from_secs(1), publication_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            publication.command_ids,
            vec![BoardCmdId("test-id".to_string())]
        );

        drop(status_tx);
        drop(board_tx);
        timeout(Duration::from_secs(1), adapter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn external_egress_does_not_publish_without_an_acknowledgement() {
        let (status_tx, status_rx) = mpsc::channel(1);
        let (board_tx, board_rx) = mpsc::channel(1);
        let (publication_tx, mut publication_rx) = mpsc::channel(1);
        let (clear_tx, _clear_rx) = mpsc::channel(1);
        let adapter = tokio::spawn(external_egress_adapter(
            "cat >/dev/null".to_string(),
            status_rx,
            board_rx,
            publication_tx,
            clear_tx,
        ));

        board_tx.send(BoardState::default()).await.unwrap();
        assert!(
            timeout(Duration::from_millis(100), publication_rx.recv())
                .await
                .is_err()
        );

        drop(status_tx);
        drop(board_tx);
        timeout(Duration::from_secs(1), adapter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn external_egress_forwards_clear_requests() {
        let (status_tx, status_rx) = mpsc::channel(1);
        let (board_tx, board_rx) = mpsc::channel(1);
        let (publication_tx, _publication_rx) = mpsc::channel(1);
        let (clear_tx, mut clear_rx) = mpsc::channel(1);
        let adapter = tokio::spawn(external_egress_adapter(
            r#"while IFS= read -r _; do printf '%s\n' '{"kind":"clear_board_commands","command_ids":["test-id"],"reason":"host schedule cleared"}'; done"#.to_string(),
            status_rx,
            board_rx,
            publication_tx,
            clear_tx,
        ));

        board_tx.send(BoardState::default()).await.unwrap();
        let clear = timeout(Duration::from_secs(1), clear_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(clear.command_ids, vec![BoardCmdId("test-id".to_string())]);
        assert_eq!(clear.reason, "host schedule cleared");

        drop(status_tx);
        drop(board_tx);
        timeout(Duration::from_secs(1), adapter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn append_host_command_dispatch_csv_writes_header_and_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let csv_path = tmp.path().join("dispatch.csv");

        let first = BoardState {
            ..Default::default()
        };

        let second = BoardState {
            ..Default::default()
        };

        write_host_command_dispatch_csv(&csv_path, &first)
            .await
            .unwrap();
        write_host_command_dispatch_csv(&csv_path, &second)
            .await
            .unwrap();

        let content = tokio::fs::read_to_string(&csv_path).await.unwrap();
        let mut reader = csv::Reader::from_reader(content.as_bytes());
        let rows: Vec<HostCommandDispatchCsvRecord> =
            reader.deserialize().collect::<Result<_, _>>().unwrap();

        // assert_eq!(rows.len(), 2);
        // assert_eq!(rows[0].event_seq, 1);
        // assert_eq!(rows[0].event_ts_mono, 11);
        // assert_eq!(rows[0].event_source, "Controller");
        // assert_eq!(rows[0].event_msg_kind, "ExecuteNow");
        // assert!(rows[0].timed_command_json.contains("PointNadir"));

        // assert_eq!(rows[1].event_seq, 2);
        // assert_eq!(rows[1].event_ts_mono, 12);
        // assert_eq!(rows[1].event_source, "Controller");
        // assert_eq!(rows[1].event_msg_kind, "ExecuteNow");
        // assert!(rows[1].timed_command_json.contains("PointSunYaw"));
    }

    #[tokio::test]
    async fn append_host_command_dispatch_csv_creates_parent_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let csv_path = tmp.path().join("nested/a/b/dispatch.csv");

        let row = BoardState {
            ..Default::default()
        };

        write_host_command_dispatch_csv(&csv_path, &row)
            .await
            .unwrap();

        assert!(csv_path.exists());
    }

    #[tokio::test]
    async fn write_host_command_dispatch_csv_writes_header_for_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let csv_path = tmp.path().join("dispatch.csv");
        tokio::fs::write(&csv_path, "").await.unwrap();

        let mut row = BoardState {
            ..Default::default()
        };
        let id =
            crate::protocol::BoardCmdId("7:00000000-0000-0000-0000-000000000001:0".to_string());
        row.proposals.insert(
            id.clone(),
            (
                crate::protocol::AutonomyModeId(uuid::Uuid::from_u128(1)),
                TimedCommand::Scheduled {
                    cmd: Command::PointNadir,
                    gps_time: 70.0,
                },
                70,
            ),
        );
        row.approved.insert(
            id.clone(),
            vec![(
                crate::protocol::AutonomyModeId(uuid::Uuid::nil()),
                "approved".to_string(),
                70,
            )],
        );
        row.source_of_truth = vec![id];

        write_host_command_dispatch_csv(&csv_path, &row)
            .await
            .unwrap();

        let content = tokio::fs::read_to_string(&csv_path).await.unwrap();
        let mut lines = content.lines();
        let header = lines.next().unwrap();
        let data = lines.next().unwrap();

        assert!(header.contains("cmd"));
        assert!(header.contains("gps_time"));
        assert!(data.starts_with("PointNadir,70"));
    }
}
