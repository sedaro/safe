use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tracing::{error, info_span, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

use crate::protocol::{
    AUTONOMY_MODE_PROTOCOL_VERSION, AutonomyModeBoardState, AutonomyModeId, AutonomyModeInput,
    AutonomyModeLifecycle, AutonomyModeOutput, BoardCmdId, BoardState, CommandEnvelope, ModeToSafe,
    SafeToMode,
};
use crate::telemetry_frame::TelemetryFrame;
use crate::transports::TransportHandle;
use crate::transports::unix::UnixTransportHandle;

fn init_mode_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .expect("valid tracing filter");
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(true)
        .with_target(true)
        .try_init();
}

pub struct ModeRuntime {
    active: bool,
    mode_id: AutonomyModeId,
    working_directory: PathBuf,
    output_tx: ModeOutputTx,
}

#[derive(Clone)]
pub struct ModeOutputTx {
    tx: mpsc::UnboundedSender<AutonomyModeOutput>,
}

impl ModeOutputTx {
    fn new(tx: mpsc::UnboundedSender<AutonomyModeOutput>) -> Self {
        Self { tx }
    }

    pub fn send_output(&self, output: AutonomyModeOutput) -> Result<()> {
        self.tx
            .send(output)
            .map_err(|e| anyhow!("mode output channel closed: {e}"))
    }

    pub async fn fault(&self, msg: impl Into<String>) -> Result<()> {
        self.send_output(AutonomyModeOutput::Fault(msg.into()))
    }

    pub async fn command(&self, env: CommandEnvelope) -> Result<()> {
        self.send_output(AutonomyModeOutput::Command(env))
    }

    pub async fn cancel_board(&self, id: BoardCmdId, reason: impl Into<String>) -> Result<()> {
        self.send_output(AutonomyModeOutput::CancelBoard {
            id,
            reason: reason.into(),
        })
    }

    pub fn lifecycle(&self, state: AutonomyModeLifecycle) -> Result<()> {
        self.send_output(AutonomyModeOutput::Lifecycle { state })
    }
}

impl ModeRuntime {
    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn mode_id(&self) -> AutonomyModeId {
        self.mode_id
    }

    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    pub fn output_tx(&self) -> ModeOutputTx {
        self.output_tx.clone()
    }

    pub async fn send_output(&mut self, output: AutonomyModeOutput) -> Result<()> {
        self.output_tx.send_output(output)
    }

    pub async fn fault(&mut self, msg: impl Into<String>) -> Result<()> {
        self.send_output(AutonomyModeOutput::Fault(msg.into()))
            .await
    }

    pub async fn command(&mut self, env: CommandEnvelope) -> Result<()> {
        self.send_output(AutonomyModeOutput::Command(env)).await
    }

    pub async fn cancel_board(&mut self, id: BoardCmdId, reason: impl Into<String>) -> Result<()> {
        self.send_output(AutonomyModeOutput::CancelBoard {
            id,
            reason: reason.into(),
        })
        .await
    }
}

#[async_trait]
pub trait ModeHandler<C>: Send
where
    C: Send + 'static,
{
    fn set_config(&mut self, config: C) -> Result<()>;

    async fn on_activate(&mut self, _runtime: &mut ModeRuntime) -> Result<()> {
        Ok(())
    }

    async fn on_deactivate(&mut self, _runtime: &mut ModeRuntime) -> Result<()> {
        Ok(())
    }

    async fn on_telemetry(&mut self, _runtime: &mut ModeRuntime, _t: TelemetryFrame) -> Result<()> {
        Ok(())
    }

    async fn on_board_snapshot(
        &mut self,
        _runtime: &mut ModeRuntime,
        _board: AutonomyModeBoardState,
    ) -> Result<()> {
        Ok(())
    }

    async fn on_shutdown(&mut self, _runtime: &mut ModeRuntime) -> Result<()> {
        Ok(())
    }
}

pub async fn run_mode<C, H>(mut handler: H) -> Result<()>
where
    C: DeserializeOwned + Default + Send + 'static,
    H: ModeHandler<C>,
{
    init_mode_tracing();

    let (endpoint_opt, config_path, mode_id_opt, working_directory_opt) =
        parse_mode_args(&env::args().collect::<Vec<_>>());
    let endpoint = resolve_endpoint(endpoint_opt.as_deref())?;
    let mode_id = resolve_mode_id(mode_id_opt.as_deref())?;
    let working_directory = resolve_working_directory(working_directory_opt.as_deref())?;
    let config = load_mode_config::<C>(config_path.as_deref()).await?;
    handler.set_config(config)?;

    let handle = UnixTransportHandle::<ModeToSafe, SafeToMode>::new(&endpoint);
    let mut stream = TransportHandle::connect(&handle)
        .await
        .map_err(|e| anyhow!(e))?;
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<AutonomyModeOutput>();

    match stream.read().await {
        Ok(SafeToMode::Hello { expected_mode }) => {
            if expected_mode != mode_id {
                return Err(anyhow!("wrong mode id in hello"));
            }
        }
        Ok(_) => return Err(anyhow!("expected initial hello from SAFE")),
        Err(e) => return Err(anyhow!(e)),
    }

    stream
        .write(ModeToSafe::Hello {
            mode: mode_id,
            protocol_version: AUTONOMY_MODE_PROTOCOL_VERSION,
        })
        .await
        .map_err(|e| anyhow!(e))?;

    let span_id = format!("mode_runtime_{}", mode_id.0);
    let span = info_span!(
        "autonomy_mode_runtime",
        mode_id = %mode_id.0,
        runtime_span = %span_id
    );
    let _guard = span.enter();

    let mut runtime = ModeRuntime {
        active: false,
        mode_id,
        working_directory,
        output_tx: ModeOutputTx::new(out_tx),
    };
    runtime.output_tx.lifecycle(AutonomyModeLifecycle::Ready)?;

    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(5));

    async fn send_fault_to_safe(
        stream: &mut Box<dyn crate::transports::Stream<SafeToMode, ModeToSafe>>,
        msg: String,
    ) {
        if let Err(write_err) = stream
            .write(ModeToSafe::Output(AutonomyModeOutput::Fault(msg.clone())))
            .await
        {
            error!(
                reason = %write_err,
                fault = %msg,
                "failed to send autonomy mode fault to SAFE"
            );
        }
    }

    loop {
        tokio::select! {
            incoming = stream.read() => {
                match incoming {
                    Ok(SafeToMode::Input(input)) => match input {
                        AutonomyModeInput::Activate => {
                            runtime.active = true;
                            if let Err(e) = handler.on_activate(&mut runtime).await {
                                let msg = format!("on_activate failed: {e:#}");
                                error!(reason = %msg, "autonomy mode handler failure");
                                send_fault_to_safe(&mut stream, msg.clone()).await;
                                return Err(anyhow!(msg));
                            }
                            runtime.output_tx.lifecycle(AutonomyModeLifecycle::Active)?;
                        }
                        AutonomyModeInput::Deactivate => {
                            runtime.active = false;
                            if let Err(e) = handler.on_deactivate(&mut runtime).await {
                                let msg = format!("on_deactivate failed: {e:#}");
                                error!(reason = %msg, "autonomy mode handler failure");
                                send_fault_to_safe(&mut stream, msg.clone()).await;
                                return Err(anyhow!(msg));
                            }
                            runtime.output_tx.lifecycle(AutonomyModeLifecycle::Inactive)?;
                        }
                        AutonomyModeInput::Restart => {
                            runtime.active = false;
                            if let Err(e) = handler.on_deactivate(&mut runtime).await {
                                let msg = format!("on_restart_deactivate failed: {e:#}");
                                error!(reason = %msg, "autonomy mode handler failure");
                                send_fault_to_safe(&mut stream, msg.clone()).await;
                                return Err(anyhow!(msg));
                            }
                            runtime.output_tx.lifecycle(AutonomyModeLifecycle::Active)?;
                            runtime.active = true;
                            if let Err(e) = handler.on_activate(&mut runtime).await {
                                let msg = format!("on_restart_activate failed: {e:#}");
                                error!(reason = %msg, "autonomy mode handler failure");
                                send_fault_to_safe(&mut stream, msg.clone()).await;
                                return Err(anyhow!(msg));
                            }
                        }
                        AutonomyModeInput::Telemetry(t) => {
                            if let Err(e) = handler.on_telemetry(&mut runtime, t).await {
                                let msg = format!("on_telemetry failed: {e:#}");
                                error!(reason = %msg, "autonomy mode handler failure");
                                send_fault_to_safe(&mut stream, msg.clone()).await;
                                return Err(anyhow!(msg));
                            }
                        }
                        AutonomyModeInput::BoardSnapshot(board) => {
                            if let Err(e) = handler.on_board_snapshot(&mut runtime, board).await {
                                let msg = format!("on_board_snapshot failed: {e:#}");
                                error!(reason = %msg, "autonomy mode handler failure");
                                send_fault_to_safe(&mut stream, msg.clone()).await;
                                return Err(anyhow!(msg));
                            }
                        }
                        AutonomyModeInput::Shutdown => {
                            if let Err(e) = handler.on_shutdown(&mut runtime).await {
                                let msg = format!("on_shutdown failed: {e:#}");
                                error!(reason = %msg, "autonomy mode handler failure");
                                send_fault_to_safe(&mut stream, msg.clone()).await;
                                return Err(anyhow!(msg));
                            }
                            runtime.output_tx.lifecycle(AutonomyModeLifecycle::Stopping)?;
                            flush_pending_outputs(&mut out_rx, &mut stream).await?;
                            break;
                        }
                    },
                    Ok(SafeToMode::Hello { .. }) => {}
                    Err(e) => {
                        warn!(reason = %e, "autonomy mode runtime stream read failed; exiting loop");
                        break;
                    }
                }
            }
            maybe_out = out_rx.recv() => {
                let Some(out) = maybe_out else {
                    break;
                };
                if let Err(e) = stream.write(ModeToSafe::Output(out)).await {
                    error!(reason = %e, "failed sending autonomy mode output to SAFE");
                    return Err(anyhow!(e));
                }
            }
            _ = heartbeat.tick() => {
                if let Err(e) = stream.write(ModeToSafe::Output(AutonomyModeOutput::Heartbeat)).await {
                    error!(reason = %e, "failed sending autonomy mode heartbeat to SAFE");
                    return Err(anyhow!(e));
                }
            }
        }
    }

    Ok(())
}

async fn flush_pending_outputs(
    out_rx: &mut mpsc::UnboundedReceiver<AutonomyModeOutput>,
    stream: &mut Box<dyn crate::transports::Stream<SafeToMode, ModeToSafe>>,
) -> Result<()> {
    loop {
        match out_rx.try_recv() {
            Ok(out) => {
                stream
                    .write(ModeToSafe::Output(out))
                    .await
                    .map_err(|e| anyhow!(e))?;
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return Ok(()),
        }
    }
}

fn parse_mode_args(
    args: &[String],
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let mut endpoint = None;
    let mut config_path = None;
    let mut mode_id = None;
    let mut working_directory = None;
    let mut idx = 0usize;
    while idx < args.len() {
        if args[idx] == "--endpoint" && idx + 1 < args.len() {
            endpoint = Some(args[idx + 1].clone());
            idx += 1;
        } else if args[idx] == "--config" && idx + 1 < args.len() {
            config_path = Some(args[idx + 1].clone());
            idx += 1;
        } else if args[idx] == "--mode-id" && idx + 1 < args.len() {
            mode_id = Some(args[idx + 1].clone());
            idx += 1;
        } else if args[idx] == "--working-directory" && idx + 1 < args.len() {
            working_directory = Some(args[idx + 1].clone());
            idx += 1;
        }
        idx += 1;
    }

    (endpoint, config_path, mode_id, working_directory)
}

fn resolve_endpoint(endpoint_arg: Option<&str>) -> Result<String> {
    endpoint_arg
        .map(str::to_string)
        .or_else(|| env::var("SAFE_MODE_ENDPOINT").ok())
        .ok_or_else(|| anyhow!("missing endpoint; pass --endpoint or SAFE_MODE_ENDPOINT"))
}

fn resolve_mode_id(mode_id_arg: Option<&str>) -> Result<AutonomyModeId> {
    let raw = mode_id_arg
        .map(str::to_string)
        .or_else(|| env::var("SAFE_MODE_ID").ok())
        .ok_or_else(|| anyhow!("missing mode id; pass --mode-id or SAFE_MODE_ID"))?;
    let parsed = Uuid::parse_str(&raw).map_err(|e| anyhow!("invalid mode id '{raw}': {e}"))?;
    Ok(AutonomyModeId(parsed))
}

fn resolve_working_directory(working_directory_arg: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = working_directory_arg {
        return Ok(PathBuf::from(path));
    }

    if let Ok(path) = env::var("SAFE_MODE_WORKING_DIRECTORY") {
        return Ok(PathBuf::from(path));
    }

    Ok(env::current_dir()?)
}

async fn load_mode_config<C>(config_path: Option<&str>) -> Result<C>
where
    C: DeserializeOwned + Default,
{
    if let Some(path) = config_path {
        let config_content = tokio::fs::read_to_string(path).await?;
        parse_mode_config_content(&config_content)
    } else {
        Ok(C::default())
    }
}

fn parse_mode_config_content<C>(config_content: &str) -> Result<C>
where
    C: DeserializeOwned + Default,
{
    serde_json::from_str::<C>(config_content).map_err(|e| anyhow!("mode config parse failed: {e}"))
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use tokio::sync::mpsc;

    use super::*;

    #[derive(Debug, Deserialize, Default, PartialEq, Eq)]
    struct TestCfg {
        #[serde(default)]
        alpha: u32,
    }

    #[test]
    fn parse_mode_args_extracts_endpoint_and_config() {
        let args = vec![
            "mode_no_images".to_string(),
            "--endpoint".to_string(),
            "/tmp/mode.sock".to_string(),
            "--config".to_string(),
            "/tmp/cfg.json".to_string(),
            "--mode-id".to_string(),
            "123e4567-e89b-12d3-a456-426614174000".to_string(),
            "--working-directory".to_string(),
            "/tmp/mode-work-dir".to_string(),
        ];

        let (endpoint, config, mode_id, working_directory) = parse_mode_args(&args);
        assert_eq!(endpoint.as_deref(), Some("/tmp/mode.sock"));
        assert_eq!(config.as_deref(), Some("/tmp/cfg.json"));
        assert_eq!(
            mode_id.as_deref(),
            Some("123e4567-e89b-12d3-a456-426614174000")
        );
        assert_eq!(working_directory.as_deref(), Some("/tmp/mode-work-dir"));
    }

    #[test]
    fn resolve_endpoint_prefers_cli_value() {
        let endpoint = resolve_endpoint(Some("/tmp/cli.sock")).unwrap();
        assert_eq!(endpoint, "/tmp/cli.sock");
    }

    #[test]
    fn parse_mode_config_content_rejects_invalid_json() {
        let cfg: Result<TestCfg> = parse_mode_config_content("{not-valid-json");
        assert!(cfg.is_err());
    }

    #[test]
    fn parse_mode_config_content_parses_valid_json() {
        let cfg: TestCfg = parse_mode_config_content("{\"alpha\":17}").expect("valid config");
        assert_eq!(cfg, TestCfg { alpha: 17 });
    }
}
