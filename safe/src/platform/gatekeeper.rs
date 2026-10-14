use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, sleep, timeout};
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
    Ready,
    Unavailable {
        reason: String,
    },
    Approve {
        request_id: u64,
        details: String,
        simulation_count: u64,
    },
    Reject {
        request_id: u64,
        reason: String,
        simulation_count: u64,
    },
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
    Approve {
        request_id: u64,
        details: String,
        #[serde(default)]
        simulation_count: u64,
    },
    Reject {
        request_id: u64,
        reason: String,
        #[serde(default)]
        simulation_count: u64,
    },
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
                let _ = out_tx.send(GatekeeperAdapterOutput::Ready).await;
                while let Some(msg) = in_rx.recv().await {
                    if let GatekeeperAdapterInput::EvaluateBatch { request_id, .. } = msg {
                        let _ = out_tx
                            .send(GatekeeperAdapterOutput::Approve {
                                request_id,
                                details: "gatekeeper disabled".to_string(),
                                simulation_count: 0,
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
            // Bound the lost-child/lost-response case. Include every configured
            // simulation in the budget so timeout means unresponsive, not merely
            // slower than one Monte Carlo sample.
            let request_timeout = cfg
                .platform
                .external_gatekeeper_request_timeout_ms
                .map(Duration::from_millis)
                .unwrap_or_else(|| {
                    let simulation = cfg
                        .gatekeeper
                        .get("simulation_timeout_secs")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(120);
                    let samples = cfg
                        .gatekeeper
                        .pointer("/monte_carlo/samples")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    let adapter = cfg
                        .gatekeeper
                        .get("input_adapter_timeout_secs")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(30);
                    Duration::from_secs(
                        simulation
                            .saturating_mul(samples.saturating_add(1))
                            .saturating_add(adapter)
                            .saturating_add(5),
                    )
                });
            tokio::spawn(external_gatekeeper_adapter(
                command,
                cfg.platform.shell_executable.clone(),
                gatekeeper_config_json,
                request_timeout,
                in_rx,
                out_tx,
            ));
            Ok((in_tx, out_rx))
        }
    }
}

async fn external_gatekeeper_adapter(
    command: String,
    shell_executable: String,
    gatekeeper_config_json: String,
    request_timeout: Duration,
    mut in_rx: mpsc::Receiver<GatekeeperAdapterInput>,
    out_tx: mpsc::Sender<GatekeeperAdapterOutput>,
) -> anyhow::Result<()> {
    // Own the child lifecycle inside SAFE. A failed session invalidates its
    // request, preserves the newest telemetry, and retries with bounded backoff.
    let mut latest_telemetry = None;
    let mut retry_delay = Duration::from_millis(100);
    loop {
        let started = Instant::now();
        match run_gatekeeper_session(
            &command,
            &shell_executable,
            &gatekeeper_config_json,
            request_timeout,
            &mut in_rx,
            &out_tx,
            &mut latest_telemetry,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                // A request belongs to exactly one child session. Once that
                // child exits or times out, discard queued evaluations and let
                // SAFE retry its durable pending board against a fresh session.
                while let Ok(input) = in_rx.try_recv() {
                    if let GatekeeperAdapterInput::Telemetry(frame) = input {
                        latest_telemetry = Some(frame);
                    }
                }
                let reason = format!("{error:#}");
                error!(%reason, "external gatekeeper failed; restarting");
                if out_tx
                    .send(GatekeeperAdapterOutput::Unavailable { reason })
                    .await
                    .is_err()
                {
                    return Ok(());
                }
            }
        }
        if in_rx.is_closed() {
            return Ok(());
        }
        // Continue consuming telemetry during backoff so the restarted child
        // receives the newest frame rather than an obsolete snapshot.
        let retry_at = Instant::now() + retry_delay;
        loop {
            tokio::select! {
                _ = out_tx.closed() => return Ok(()),
                _ = tokio::time::sleep_until(retry_at) => break,
                input = in_rx.recv() => match input {
                    Some(GatekeeperAdapterInput::Telemetry(frame)) => latest_telemetry = Some(frame),
                    Some(GatekeeperAdapterInput::EvaluateBatch { .. }) => {},
                    None => return Ok(()),
                }
            }
        }
        retry_delay = if started.elapsed() >= Duration::from_secs(30) {
            Duration::from_millis(100)
        } else {
            retry_delay.saturating_mul(2).min(Duration::from_secs(30))
        };
    }
}

async fn write_gatekeeper_input(
    writer: &mut tokio::process::ChildStdin,
    wire: &GatekeeperWireInput,
) -> anyhow::Result<()> {
    let line = serde_json::to_string(wire)?;
    timeout(Duration::from_secs(5), async {
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await
    })
    .await
    .map_err(|_| anyhow::anyhow!("gatekeeper stdin write timed out"))??;
    Ok(())
}

async fn run_gatekeeper_session(
    command: &str,
    shell_executable: &str,
    gatekeeper_config_json: &str,
    request_timeout: Duration,
    in_rx: &mut mpsc::Receiver<GatekeeperAdapterInput>,
    out_tx: &mpsc::Sender<GatekeeperAdapterOutput>,
    latest_telemetry: &mut Option<TelemetryFrame>,
) -> anyhow::Result<()> {
    info!(%command, "platform gatekeeper adapter `external` started");
    let mut child = Command::new(shell_executable)
        .arg("-lc")
        .arg(command)
        .env("SAFE_GATEKEEPER_CONFIG_JSON", gatekeeper_config_json)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let Some(stdin) = child.stdin.take() else {
        anyhow::bail!("external gatekeeper process has no stdin");
    };
    let Some(stdout) = child.stdout.take() else {
        anyhow::bail!("external gatekeeper process has no stdout");
    };
    let Some(stderr) = child.stderr.take() else {
        anyhow::bail!("external gatekeeper process has no stderr");
    };

    let mut writer = stdin;
    let mut lines = BufReader::new(stdout).lines();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => info!(stream = "gatekeeper_stderr", "{line}"),
                Ok(None) => break,
                Err(error) => {
                    error!(%error, "gatekeeper stderr read failed");
                    break;
                }
            }
        }
    });
    if let Some(frame) = latest_telemetry.clone() {
        write_gatekeeper_input(&mut writer, &GatekeeperWireInput::Telemetry { frame }).await?;
    }
    if out_tx.send(GatekeeperAdapterOutput::Ready).await.is_err() {
        return Ok(());
    }
    let mut active_request: Option<(u64, Instant)> = None;
    loop {
        let deadline = active_request
            .map(|(_, deadline)| deadline)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(86400));
        tokio::select! {
            _ = out_tx.closed() => return Ok(()),
            input = in_rx.recv() => {
                let Some(input) = input else { return Ok(()); };
                let wire = match input {
                    GatekeeperAdapterInput::Telemetry(frame) => {
                        *latest_telemetry = Some(frame.clone());
                        // An evaluation already owns a frozen input. Coalesce
                        // updates instead of filling the child's work queue.
                        if active_request.is_some() { continue; }
                        GatekeeperWireInput::Telemetry { frame }
                    }
                    GatekeeperAdapterInput::EvaluateBatch { request_id, board, candidate_command_ids } => {
                        if active_request.is_some() {
                            anyhow::bail!("overlapping gatekeeper evaluations");
                        }
                        if let Some(frame) = latest_telemetry.clone() {
                            write_gatekeeper_input(&mut writer, &GatekeeperWireInput::Telemetry { frame }).await?;
                        }
                        active_request = Some((request_id, Instant::now() + request_timeout));
                        GatekeeperWireInput::EvaluateBatch { request_id, board, candidate_command_ids }
                    }
                };
                write_gatekeeper_input(&mut writer, &wire).await?;
            }
            line = lines.next_line() => {
            let Some(line) = line? else { anyhow::bail!("gatekeeper closed stdout before session completion"); };
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
                    simulation_count,
                }) => {
                    if active_request.map(|(id, _)| id) != Some(request_id) { continue; }
                    active_request = None;
                    if out_tx
                        .send(GatekeeperAdapterOutput::Approve {
                            request_id,
                            details,
                            simulation_count,
                        })
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                Ok(GatekeeperWireOutput::Reject {
                    request_id,
                    reason,
                    simulation_count,
                }) => {
                    if active_request.map(|(id, _)| id) != Some(request_id) { continue; }
                    active_request = None;
                    if out_tx
                        .send(GatekeeperAdapterOutput::Reject {
                            request_id,
                            reason,
                            simulation_count,
                        })
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                Err(e) => {
                    anyhow::bail!("invalid external gatekeeper response: {e}; line={trimmed}");
                }
            }
            }
            status = child.wait() => {
                anyhow::bail!("gatekeeper process exited: {:?}", status?);
            }
            _ = tokio::time::sleep_until(deadline), if active_request.is_some() => {
                // A live child can still stop answering. Treat that identically
                // to exit: fail closed, restart it, and retry pending work.
                anyhow::bail!("gatekeeper evaluation {:?} timed out after {:?}", active_request.map(|(id, _)| id), request_timeout);
            }
        }
    }
}
