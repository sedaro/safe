use safe::protocol::{BoardCmdId, BoardState};
use safe::telemetry_frame::TelemetryFrame;
use safe_gatekeeper::Gatekeeper;
use safe_gatekeeper::gatekeeper_types::{GatekeeperConfig, GatekeeperInput, GatekeeperOutput};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireInput {
    Telemetry {
        frame: TelemetryFrame,
    },
    EvaluateBatch {
        request_id: u64,
        board: BoardState,
        candidate_command_ids: Vec<BoardCmdId>,
    },
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireOutput {
    Approve { request_id: u64, details: String },
    Reject { request_id: u64, reason: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(true)
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    let config = std::env::var("SAFE_GATEKEEPER_CONFIG_JSON")
        .ok()
        .map(|raw| serde_json::from_str::<GatekeeperConfig>(&raw))
        .transpose()?
        .unwrap_or_default();

    let (in_tx, in_rx) = tokio::sync::mpsc::channel::<GatekeeperInput>(1024);
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<GatekeeperOutput>(1024);
    tokio::spawn(async move {
        Box::new(Gatekeeper::new(config)).start(in_rx, out_tx).await;
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    loop {
        tokio::select! {
            maybe_line = lines.next_line() => {
                let Some(line) = maybe_line? else { break; };
                if line.trim().is_empty() {
                    continue;
                }

                let msg = match serde_json::from_str::<WireInput>(&line)? {
                    WireInput::Telemetry { frame } => GatekeeperInput::Telemetry(frame),
                    WireInput::EvaluateBatch { request_id, board, candidate_command_ids } => {
                        GatekeeperInput::EvaluateBatch { request_id, board, candidate_command_ids }
                    }
                };
                if in_tx.send(msg).await.is_err() {
                    break;
                }
            }
            maybe_out = out_rx.recv() => {
                let Some(out) = maybe_out else { break; };
                let wire = match out {
                    GatekeeperOutput::Approve { request_id, details } => {
                        WireOutput::Approve { request_id, details }
                    }
                    GatekeeperOutput::Reject { request_id, reason } => {
                        WireOutput::Reject { request_id, reason }
                    }
                };
                stdout.write_all(serde_json::to_string(&wire)?.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
            }
        }
    }

    Ok(())
}
