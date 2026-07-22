use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::config::Config;
use crate::protocol::{BoardCmdId, BoardState};
use crate::telemetry_frame::TelemetryFrame;

#[derive(Debug, Clone)]
pub enum GatekeeperAdapterInput {
    Telemetry(TelemetryFrame),
    EvaluateBatch {
        request_id: u64,
        board: BoardState,
        candidate_command_ids: Vec<BoardCmdId>,
    },
}

#[derive(Debug, Clone)]
pub enum GatekeeperAdapterOutput {
    Approve { request_id: u64, details: String },
    Reject { request_id: u64, reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatekeeperAdapterKind {
    Disabled,
    External,
}

impl GatekeeperAdapterKind {
    pub fn from_config(name: &str) -> anyhow::Result<Self> {
        match name {
            "disabled" => Ok(Self::Disabled),
            "external" => Ok(Self::External),
            _ => anyhow::bail!("unsupported gatekeeper adapter: {name}"),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum GatekeeperWireInput {
    Telemetry {
        frame: TelemetryFrame,
    },
    EvaluateBatch {
        request_id: u64,
        board: BoardState,
        candidate_command_ids: Vec<BoardCmdId>,
    },
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum GatekeeperWireOutput {
    Approve { request_id: u64, details: String },
    Reject { request_id: u64, reason: String },
}

pub fn spawn_gatekeeper_adapter(
    cfg: &Config,
) -> anyhow::Result<(
    mpsc::Sender<GatekeeperAdapterInput>,
    mpsc::Receiver<GatekeeperAdapterOutput>,
)> {
    let kind = GatekeeperAdapterKind::from_config(&cfg.platform.gatekeeper_adapter)?;

    match kind {
        GatekeeperAdapterKind::Disabled => {
            let (in_tx, mut in_rx) = mpsc::channel::<GatekeeperAdapterInput>(1024);
            let (out_tx, out_rx) = mpsc::channel::<GatekeeperAdapterOutput>(1024);

            tokio::spawn(async move {
                while let Some(msg) = in_rx.recv().await {
                    if let GatekeeperAdapterInput::EvaluateBatch { request_id, .. } = msg {
                        let _ = out_tx
                            .send(GatekeeperAdapterOutput::Approve {
                                request_id,
                                details: "gatekeeper disabled".to_string(),
                            })
                            .await;
                    }
                }
            });

            Ok((in_tx, out_rx))
        }
        GatekeeperAdapterKind::External => {
            let command = cfg
                .platform
                .external_gatekeeper_command
                .clone()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "gatekeeper adapter external selected, but platform.external_gatekeeper_command is not configured"
                    )
                })?;

            let (in_tx, in_rx) = mpsc::channel::<GatekeeperAdapterInput>(1024);
            let (out_tx, out_rx) = mpsc::channel::<GatekeeperAdapterOutput>(1024);
            let gatekeeper_config_json = serde_json::to_string(&cfg.gatekeeper)?;
            tokio::spawn(external_gatekeeper_adapter(
                command,
                gatekeeper_config_json,
                in_rx,
                out_tx,
            ));
            Ok((in_tx, out_rx))
        }
    }
}

async fn external_gatekeeper_adapter(
    command: String,
    gatekeeper_config_json: String,
    mut in_rx: mpsc::Receiver<GatekeeperAdapterInput>,
    out_tx: mpsc::Sender<GatekeeperAdapterOutput>,
) -> anyhow::Result<()> {
    info!(%command, "platform gatekeeper adapter `external` started");

    let mut child = Command::new("bash")
        .arg("-lc")
        .arg(command)
        .env("SAFE_GATEKEEPER_CONFIG_JSON", gatekeeper_config_json)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;

    let Some(stdin) = child.stdin.take() else {
        anyhow::bail!("external gatekeeper process has no stdin");
    };
    let Some(stdout) = child.stdout.take() else {
        anyhow::bail!("external gatekeeper process has no stdout");
    };

    let mut writer = stdin;
    let mut lines = BufReader::new(stdout).lines();
    let out_tx_reader = out_tx.clone();

    let reader_task = tokio::spawn(async move {
        while let Some(line) = lines.next_line().await? {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Gatekeeper wire protocol is JSONL on stdout. Ignore non-JSON lines
            // (for example, when users run via `cargo run` and build/log output
            // is merged into stdout).
            if !trimmed.starts_with('{') {
                continue;
            }

            match serde_json::from_str::<GatekeeperWireOutput>(trimmed) {
                Ok(GatekeeperWireOutput::Approve {
                    request_id,
                    details,
                }) => {
                    if out_tx_reader
                        .send(GatekeeperAdapterOutput::Approve {
                            request_id,
                            details,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(GatekeeperWireOutput::Reject { request_id, reason }) => {
                    if out_tx_reader
                        .send(GatekeeperAdapterOutput::Reject { request_id, reason })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => {
                    error!("invalid external gatekeeper json value: {e}; line={trimmed}");
                }
            }
        }

        anyhow::Result::<()>::Ok(())
    });

    while let Some(msg) = in_rx.recv().await {
        let wire = match msg {
            GatekeeperAdapterInput::Telemetry(frame) => GatekeeperWireInput::Telemetry { frame },
            GatekeeperAdapterInput::EvaluateBatch {
                request_id,
                board,
                candidate_command_ids,
            } => GatekeeperWireInput::EvaluateBatch {
                request_id,
                board,
                candidate_command_ids,
            },
        };

        let line = serde_json::to_string(&wire)?;
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }

    let _ = reader_task.await;
    Ok(())
}
