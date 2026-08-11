use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::sync::{RwLock, mpsc};
use tokio::time::{Duration, sleep, timeout};
use tracing::{error, info, info_span, warn};

use crate::protocol::{
    AUTONOMY_MODE_PROTOCOL_VERSION, AutonomyModeBoardState, AutonomyModeLifecycle,
};
use crate::runtime::{ModeConnectionState, ModeHandlerState, ModeRuntimeStatus};
use crate::sandbox::sandbox::{SafeSandbox, Sandbox, SandboxConfig, SandboxResources};
use crate::telemetry_frame::TelemetryFrame;
use crate::transports::{Transport, UnixTransport};
use crate::{AutonomyModeId, AutonomyModeInput, AutonomyModeOutput, ModeToSafe, SafeToMode};
use crate::{BoardState, RuntimePaths};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomyModeConfig {
    pub id: AutonomyModeId,
    pub priority: u8,
    pub enabled: bool,
    pub bin_path: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub sandbox_resources: SandboxResources,
    #[serde(default)]
    pub persist_work_dir: bool,
    #[serde(default)]
    pub mode_config: serde_json::Value,
}

#[derive(Debug)]
struct AutonomyModeHandle {
    in_tx: mpsc::Sender<AutonomyModeInput>,
}

pub struct Router {
    handles: HashMap<AutonomyModeId, AutonomyModeHandle>,
    configs: HashMap<AutonomyModeId, AutonomyModeConfig>,
    out_tx: mpsc::Sender<(AutonomyModeId, AutonomyModeOutput)>,
    out_rx: mpsc::Receiver<(AutonomyModeId, AutonomyModeOutput)>,
    desired_active: Arc<RwLock<Option<AutonomyModeId>>>,
    connected_modes: Arc<RwLock<HashSet<AutonomyModeId>>>,
    mode_statuses: Arc<RwLock<HashMap<AutonomyModeId, ModeRuntimeStatus>>>,
    runtime_paths: RuntimePaths,
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

async fn set_mode_connection_status(
    statuses: &Arc<RwLock<HashMap<AutonomyModeId, ModeRuntimeStatus>>>,
    id: AutonomyModeId,
    connection: ModeConnectionState,
    detail: Option<String>,
) {
    let mut statuses = statuses.write().await;
    let status = statuses.entry(id).or_default();
    status.connection = connection;
    status.last_transition_unix_ms = Some(now_unix_ms());
    status.detail = detail;
}

async fn set_mode_handler_status(
    statuses: &Arc<RwLock<HashMap<AutonomyModeId, ModeRuntimeStatus>>>,
    id: AutonomyModeId,
    handler: ModeHandlerState,
) {
    let mut statuses = statuses.write().await;
    let status = statuses.entry(id).or_default();
    status.handler = Some(handler);
    status.last_transition_unix_ms = Some(now_unix_ms());
    status.detail = None;
}

async fn note_mode_heartbeat(
    statuses: &Arc<RwLock<HashMap<AutonomyModeId, ModeRuntimeStatus>>>,
    id: AutonomyModeId,
) {
    let mut statuses = statuses.write().await;
    statuses.entry(id).or_default().last_heartbeat_unix_ms = Some(now_unix_ms());
}

fn should_activate_on_connect(
    mode_id: AutonomyModeId,
    desired_active: Option<AutonomyModeId>,
) -> bool {
    desired_active == Some(mode_id)
}

impl Router {
    async fn spawn_mode(&mut self, cfg: AutonomyModeConfig) -> anyhow::Result<()> {
        let socket_dir = self.runtime_paths.state.join("modes");
        fs::create_dir_all(&socket_dir).await?;

        let mode_dir = socket_dir.join(format!("{:?}", cfg.id.0).to_lowercase());
        if !cfg.persist_work_dir {
            match fs::remove_dir_all(&mode_dir).await {
                _ => {}
            }
        }
        fs::create_dir_all(&mode_dir).await?;

        let socket_path = mode_dir.join("ipc.sock");
        let config_path = mode_dir.join("mode-config.json");
        let config_json = serde_json::to_vec_pretty(&cfg.mode_config)
            .map_err(|e| anyhow::anyhow!("serialize mode config for {:?}: {e}", cfg.id))?;
        fs::write(&config_path, config_json).await?;

        let mut launch_args = cfg.args.clone();
        launch_args.push("--endpoint".to_string());
        launch_args.push(socket_path.to_string_lossy().to_string());
        launch_args.push("--config".to_string());
        launch_args.push(config_path.to_string_lossy().to_string());
        launch_args.push("--mode-id".to_string());
        launch_args.push(cfg.id.to_string());

        let (in_tx, in_rx) = mpsc::channel::<AutonomyModeInput>(1024);
        tokio::spawn(run_mode_supervisor(
            cfg.id,
            mode_dir,
            cfg.bin_path.clone(),
            launch_args,
            cfg.sandbox_resources.clone(),
            cfg.persist_work_dir,
            in_rx,
            self.out_tx.clone(),
            self.desired_active.clone(),
            self.connected_modes.clone(),
            self.mode_statuses.clone(),
        ));

        self.handles.insert(cfg.id, AutonomyModeHandle { in_tx });
        set_mode_connection_status(
            &self.mode_statuses,
            cfg.id,
            ModeConnectionState::Starting,
            None,
        )
        .await;
        self.configs.insert(cfg.id, cfg);
        Ok(())
    }

    pub async fn start(
        configs: Vec<AutonomyModeConfig>,
        runtime_paths: &RuntimePaths,
    ) -> anyhow::Result<Self> {
        let (out_tx, out_rx) = mpsc::channel::<(AutonomyModeId, AutonomyModeOutput)>(1024);
        let desired_active = Arc::new(RwLock::new(None));
        let connected_modes = Arc::new(RwLock::new(HashSet::new()));
        let mode_statuses = Arc::new(RwLock::new(HashMap::new()));
        let mut router = Self {
            handles: HashMap::new(),
            configs: HashMap::new(),
            out_rx,
            desired_active,
            connected_modes,
            mode_statuses,
            runtime_paths: runtime_paths.clone(),
            out_tx,
        };

        for cfg in configs {
            router.spawn_mode(cfg).await?;
        }

        Ok(router)
    }

    pub async fn reconcile_configs(
        &mut self,
        new_configs: Vec<AutonomyModeConfig>,
    ) -> anyhow::Result<()> {
        let new_by_id: HashMap<AutonomyModeId, AutonomyModeConfig> =
            new_configs.into_iter().map(|cfg| (cfg.id, cfg)).collect();

        let existing_ids: Vec<AutonomyModeId> = self.configs.keys().copied().collect();
        let mut remove_ids = Vec::new();
        let mut restart_ids = Vec::new();

        for id in existing_ids {
            match new_by_id.get(&id) {
                None => remove_ids.push(id),
                Some(new_cfg) => {
                    if let Some(old_cfg) = self.configs.get(&id)
                        && mode_restart_required(old_cfg, new_cfg)
                    {
                        restart_ids.push(id);
                    }
                }
            }
        }

        for id in remove_ids {
            self.remove_mode(id).await;
        }

        for id in restart_ids {
            self.remove_mode(id).await;
            if let Some(cfg) = new_by_id.get(&id) {
                self.spawn_mode(cfg.clone()).await?;
            }
        }

        for (id, cfg) in new_by_id {
            if !self.configs.contains_key(&id) {
                self.spawn_mode(cfg).await?;
            }
        }

        Ok(())
    }

    async fn remove_mode(&mut self, id: AutonomyModeId) {
        self.configs.remove(&id);
        self.connected_modes.write().await.remove(&id);
        set_mode_connection_status(&self.mode_statuses, id, ModeConnectionState::Stopped, None)
            .await;
        if let Some(handle) = self.handles.remove(&id) {
            let _ = handle.in_tx.send(AutonomyModeInput::Shutdown).await;
        }
    }

    pub async fn send_board_snapshot_to_all(&self, board: BoardState) {
        let board_state = AutonomyModeBoardState {
            source_of_truth: board.source_of_truth.clone(),
            proposals: board.proposals.clone(),
            rejected: board.rejected.clone(),
            approved: board.approved.clone(),
        };
        for h in self.handles.values() {
            let _ = h
                .in_tx
                .send(AutonomyModeInput::BoardSnapshot(board_state.clone()))
                .await;
        }
    }

    pub async fn send_telemetry_to_all(&self, t: TelemetryFrame) {
        for h in self.handles.values() {
            let _ = h.in_tx.send(AutonomyModeInput::Telemetry(t.clone())).await;
        }
    }

    pub async fn set_active(
        &self,
        old_active: Option<AutonomyModeId>,
        new_active: Option<AutonomyModeId>,
    ) {
        *self.desired_active.write().await = new_active;
        let connected = self.connected_modes.read().await.clone();

        if old_active == new_active {
            return;
        }

        if let Some(old_id) = old_active {
            if connected.contains(&old_id)
                && let Some(h) = self.handles.get(&old_id)
            {
                let _ = h.in_tx.send(AutonomyModeInput::Deactivate).await;
            }
        }
        if let Some(new_id) = new_active {
            if connected.contains(&new_id)
                && let Some(h) = self.handles.get(&new_id)
            {
                let _ = h.in_tx.send(AutonomyModeInput::Activate).await;
            }
        }
    }

    pub async fn set_desired_active(&self, new_active: Option<AutonomyModeId>) {
        *self.desired_active.write().await = new_active;
    }

    pub fn try_recv_output(&mut self) -> Option<(AutonomyModeId, AutonomyModeOutput)> {
        self.out_rx.try_recv().ok()
    }

    pub async fn shutdown_all(&self) {
        for h in self.handles.values() {
            let _ = h.in_tx.send(AutonomyModeInput::Shutdown).await;
        }
    }

    pub async fn send_input_to(&self, id: AutonomyModeId, input: AutonomyModeInput) -> bool {
        if let Some(h) = self.handles.get(&id) {
            return h.in_tx.send(input).await.is_ok();
        }
        false
    }

    pub async fn mode_statuses(&self) -> HashMap<AutonomyModeId, ModeRuntimeStatus> {
        const HEARTBEAT_TIMEOUT_MS: u64 = 15_000;

        let now = now_unix_ms();
        let mut statuses = self.mode_statuses.read().await.clone();
        for status in statuses.values_mut() {
            if matches!(status.connection, ModeConnectionState::Connected) {
                let reference = status
                    .last_heartbeat_unix_ms
                    .or(status.last_transition_unix_ms);
                if reference.is_some_and(|ts| now.saturating_sub(ts) > HEARTBEAT_TIMEOUT_MS) {
                    status.connection = ModeConnectionState::Unresponsive;
                    status.detail = Some("heartbeat overdue".to_string());
                }
            }
        }
        statuses
    }
}

fn mode_restart_required(old_cfg: &AutonomyModeConfig, new_cfg: &AutonomyModeConfig) -> bool {
    old_cfg.bin_path != new_cfg.bin_path
        || old_cfg.args != new_cfg.args
        || old_cfg.sandbox_resources.cpu != new_cfg.sandbox_resources.cpu
        || old_cfg.sandbox_resources.memory != new_cfg.sandbox_resources.memory
        || old_cfg.sandbox_resources.disk != new_cfg.sandbox_resources.disk
        || old_cfg.persist_work_dir != new_cfg.persist_work_dir
        || old_cfg.mode_config != new_cfg.mode_config
}

async fn run_mode_supervisor(
    mode_id: AutonomyModeId,
    mode_dir: PathBuf,
    bin_path: PathBuf,
    args: Vec<String>,
    sandbox_resources: SandboxResources,
    persist_work_dir: bool,
    mut in_rx: mpsc::Receiver<AutonomyModeInput>,
    out_tx: mpsc::Sender<(AutonomyModeId, AutonomyModeOutput)>,
    desired_active: Arc<RwLock<Option<AutonomyModeId>>>,
    connected_modes: Arc<RwLock<HashSet<AutonomyModeId>>>,
    mode_statuses: Arc<RwLock<HashMap<AutonomyModeId, ModeRuntimeStatus>>>,
) {
    let span = info_span!("router", mode_id = %mode_id.0);
    let _guard = span.enter();

    let mut backoff_ms = 250u64;
    let mut stopping = false;
    let socket_path = mode_dir.join("ipc.sock");

    while !stopping {
        set_mode_connection_status(
            &mode_statuses,
            mode_id,
            ModeConnectionState::Connecting,
            None,
        )
        .await;
        let transport = match UnixTransport::<ModeToSafe, SafeToMode>::new(
            socket_path.to_string_lossy().as_ref(),
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                error!(mode_id = %mode_id.0, "failed to create socket: {e}");
                set_mode_connection_status(
                    &mode_statuses,
                    mode_id,
                    ModeConnectionState::Connecting,
                    Some(e.to_string()),
                )
                .await;
                sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(10_000);
                continue;
            }
        };

        let sandbox_config = SandboxConfig {
            command: bin_path.clone(),
            args: args.clone(),
            resources: sandbox_resources.clone(),
            persist_work_dir,
        };
        let mut sandbox = SafeSandbox::new(mode_id, sandbox_config, Some(mode_dir.clone()));
        let mut launch_args = args.clone();
        launch_args.push("--working-directory".to_string());
        launch_args.push(sandbox.get_work_dir().to_string_lossy().to_string());

        let _sandbox_jh = match sandbox.start(Some(launch_args)).await {
            Ok(h) => h,
            Err(e) => {
                error!(mode_id = %mode_id.0, "failed to launch sandbox ({bin_path:?}): {e}");
                set_mode_connection_status(
                    &mode_statuses,
                    mode_id,
                    ModeConnectionState::Starting,
                    Some(e.to_string()),
                )
                .await;
                sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(10_000);
                continue;
            }
        };

        let mut transport = transport;
        let accepted = timeout(Duration::from_secs(5), transport.accept()).await;
        let mut stream = match accepted {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                error!(mode_id = %mode_id.0, "failed to accept connection: {e}");
                set_mode_connection_status(
                    &mode_statuses,
                    mode_id,
                    ModeConnectionState::Connecting,
                    Some(e.to_string()),
                )
                .await;
                let _ = sandbox.stop().await;
                sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(10_000);
                continue;
            }
            Err(_) => {
                error!(mode_id = %mode_id.0, "timed out waiting for connection");
                set_mode_connection_status(
                    &mode_statuses,
                    mode_id,
                    ModeConnectionState::Connecting,
                    Some("timed out waiting for connection".to_string()),
                )
                .await;
                let _ = sandbox.stop().await;
                sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(10_000);
                continue;
            }
        };

        if let Err(e) = stream
            .write(SafeToMode::Hello {
                expected_mode: mode_id,
            })
            .await
        {
            error!(mode_id = %mode_id.0, "failed to send hello: {e}");
            set_mode_connection_status(
                &mode_statuses,
                mode_id,
                ModeConnectionState::Connecting,
                Some(e.to_string()),
            )
            .await;
            let _ = sandbox.stop().await;
            sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(10_000);
            continue;
        }

        match stream.read().await {
            Ok(ModeToSafe::Hello {
                mode,
                protocol_version,
            }) if mode == mode_id && protocol_version == AUTONOMY_MODE_PROTOCOL_VERSION => {
                connected_modes.write().await.insert(mode_id);
                set_mode_connection_status(
                    &mode_statuses,
                    mode_id,
                    ModeConnectionState::Connected,
                    None,
                )
                .await;
                info!(mode_id = %mode_id.0, "connected to autonomy mode");
            }
            Ok(other) => {
                warn!(mode_id = %mode_id.0, "unexpected handshake: {other:?}");
                set_mode_connection_status(
                    &mode_statuses,
                    mode_id,
                    ModeConnectionState::Connecting,
                    Some(format!("unexpected handshake: {other:?}")),
                )
                .await;
                let _ = sandbox.stop().await;
                sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(10_000);
                continue;
            }
            Err(e) => {
                error!(mode_id = %mode_id.0, "failed to read handshake: {e}");
                set_mode_connection_status(
                    &mode_statuses,
                    mode_id,
                    ModeConnectionState::Connecting,
                    Some(e.to_string()),
                )
                .await;
                let _ = sandbox.stop().await;
                sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(10_000);
                continue;
            }
        }

        if should_activate_on_connect(mode_id, *desired_active.read().await) {
            info!(mode_id = %mode_id.0, "sending activate on connect");
            if let Err(e) = stream
                .write(SafeToMode::Input(AutonomyModeInput::Activate))
                .await
            {
                error!(mode_id = %mode_id.0, "failed to send activate on connect: {e}");
                connected_modes.write().await.remove(&mode_id);
                set_mode_connection_status(
                    &mode_statuses,
                    mode_id,
                    ModeConnectionState::Disconnected,
                    Some(e.to_string()),
                )
                .await;
                let _ = sandbox.stop().await;
                sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(10_000);
                continue;
            }
        }

        backoff_ms = 250;
        loop {
            tokio::select! {
                maybe_input = in_rx.recv() => {
                    let Some(input) = maybe_input else {
                        stopping = true;
                        break;
                    };

                    let is_shutdown = matches!(input, AutonomyModeInput::Shutdown);
                    if let Err(e) = stream.write(SafeToMode::Input(input)).await {
                        error!(mode_id = %mode_id.0, "failed writing input to mode: {e}");
                        break;
                    }
                    if is_shutdown {
                        stopping = true;
                        break;
                    }
                }
                read_res = stream.read() => {
                    match read_res {
                        Ok(ModeToSafe::Output(out)) => {
                            match &out {
                                AutonomyModeOutput::Lifecycle { state } => {
                                    let handler = match state {
                                        AutonomyModeLifecycle::Ready => ModeHandlerState::Ready,
                                        AutonomyModeLifecycle::Active => ModeHandlerState::Active,
                                        AutonomyModeLifecycle::Inactive => ModeHandlerState::Inactive,
                                        AutonomyModeLifecycle::Stopping => ModeHandlerState::Stopping,
                                    };
                                    set_mode_handler_status(&mode_statuses, mode_id, handler).await;
                                }
                                AutonomyModeOutput::Heartbeat => {
                                    note_mode_heartbeat(&mode_statuses, mode_id).await;
                                }
                                AutonomyModeOutput::Fault(reason) => {
                                    set_mode_handler_status(&mode_statuses, mode_id, ModeHandlerState::Faulted).await;
                                    set_mode_connection_status(
                                        &mode_statuses,
                                        mode_id,
                                        ModeConnectionState::Faulted,
                                        Some(reason.clone()),
                                    )
                                    .await;
                                }
                                AutonomyModeOutput::Command(_) | AutonomyModeOutput::CancelBoard { .. } => {}
                            }
                            if let Some(active_mode_id) = desired_active.read().await.as_ref() {
                                // if *active_mode_id == mode_id {
                                let _ = out_tx.send((mode_id, out)).await;
                                // } else {
                                //     warn!(mode_id = %mode_id.0, "received output from non-active mode: {out:?}");
                                // }
                            } else {
                                warn!(mode_id = %mode_id.0, "received output while no active mode: {out:?}");
                            }
                        }
                        Ok(ModeToSafe::Hello { .. }) => {}
                        Err(e) => {
                            warn!(mode_id = %mode_id.0, "autonomy mode disconnected: {e}");
                            connected_modes.write().await.remove(&mode_id);
                            set_mode_connection_status(
                                &mode_statuses,
                                mode_id,
                                ModeConnectionState::Disconnected,
                                Some(e.to_string()),
                            )
                            .await;
                            break;
                        }
                    }
                }
            }
        }

        if stopping {
            connected_modes.write().await.remove(&mode_id);
            set_mode_connection_status(&mode_statuses, mode_id, ModeConnectionState::Stopped, None)
                .await;
            let _ = sandbox.stop().await;
            break;
        }

        connected_modes.write().await.remove(&mode_id);
        set_mode_connection_status(
            &mode_statuses,
            mode_id,
            ModeConnectionState::Disconnected,
            None,
        )
        .await;
        let _ = sandbox.stop().await;
        sleep(Duration::from_millis(backoff_ms)).await;
        backoff_ms = (backoff_ms * 2).min(10_000);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tokio::sync::mpsc;
    use uuid::Uuid;

    use super::*;
    use crate::sandbox::sandbox::SandboxResources;

    fn mk_config(id: u128, priority: u8) -> AutonomyModeConfig {
        AutonomyModeConfig {
            id: AutonomyModeId(Uuid::from_u128(id)),
            priority,
            enabled: true,
            bin_path: PathBuf::from("/tmp/mode-bin"),
            args: vec!["--foo".to_string()],
            sandbox_resources: SandboxResources {
                cpu: 90.0,
                memory: 1_000_000,
                disk: 1_000_000,
            },
            persist_work_dir: false,
            mode_config: serde_json::json!({"k": "v"}),
        }
    }

    fn mk_manager_with_channels() -> (
        Router,
        mpsc::Receiver<AutonomyModeInput>,
        mpsc::Receiver<AutonomyModeInput>,
        mpsc::Sender<(AutonomyModeId, AutonomyModeOutput)>,
    ) {
        let (in_tx_1, in_rx_1) = mpsc::channel(8);
        let (in_tx_2, in_rx_2) = mpsc::channel(8);
        let (out_tx, out_rx) = mpsc::channel(8);
        let desired_active = Arc::new(RwLock::new(None));
        let connected_modes = Arc::new(RwLock::new(HashSet::from([
            AutonomyModeId(Uuid::from_u128(1)),
            AutonomyModeId(Uuid::from_u128(2)),
        ])));

        let manager = Router {
            handles: HashMap::from([
                (
                    AutonomyModeId(Uuid::from_u128(1)),
                    AutonomyModeHandle { in_tx: in_tx_1 },
                ),
                (
                    AutonomyModeId(Uuid::from_u128(2)),
                    AutonomyModeHandle { in_tx: in_tx_2 },
                ),
            ]),
            configs: HashMap::from([
                (AutonomyModeId(Uuid::from_u128(1)), mk_config(1, 1)),
                (AutonomyModeId(Uuid::from_u128(2)), mk_config(2, 2)),
            ]),
            out_tx: out_tx.clone(),
            out_rx,
            desired_active,
            connected_modes,
            mode_statuses: Arc::new(RwLock::new(HashMap::new())),
            runtime_paths: RuntimePaths::default(),
        };

        (manager, in_rx_1, in_rx_2, out_tx)
    }

    #[tokio::test]
    async fn set_active_sends_deactivate_then_activate() {
        let (manager, mut no_images_rx, mut hive_rx, _out_tx) = mk_manager_with_channels();
        let no_images = AutonomyModeId(Uuid::from_u128(1));
        let hive_mast = AutonomyModeId(Uuid::from_u128(2));

        manager.set_active(Some(no_images), Some(hive_mast)).await;

        assert!(matches!(
            no_images_rx.recv().await,
            Some(AutonomyModeInput::Deactivate)
        ));
        assert!(matches!(
            hive_rx.recv().await,
            Some(AutonomyModeInput::Activate)
        ));
    }

    #[tokio::test]
    async fn send_input_to_routes_restart_to_target_mode() {
        let (manager, mut no_images_rx, _hive_rx, _out_tx) = mk_manager_with_channels();
        let no_images = AutonomyModeId(Uuid::from_u128(1));

        assert!(
            manager
                .send_input_to(no_images, AutonomyModeInput::Restart)
                .await
        );
        assert!(matches!(
            no_images_rx.recv().await,
            Some(AutonomyModeInput::Restart)
        ));
    }

    #[tokio::test]
    async fn set_active_persists_desired_active_for_late_connects() {
        let (manager, mut no_images_rx, _hive_rx, _out_tx) = mk_manager_with_channels();
        let no_images = AutonomyModeId(Uuid::from_u128(1));

        manager.set_active(None, Some(no_images)).await;

        assert_eq!(*manager.desired_active.read().await, Some(no_images));
        assert!(matches!(
            no_images_rx.recv().await,
            Some(AutonomyModeInput::Activate)
        ));
    }

    #[tokio::test]
    async fn set_active_does_not_send_activate_to_disconnected_mode() {
        let (manager, mut no_images_rx, _hive_rx, _out_tx) = mk_manager_with_channels();
        let no_images = AutonomyModeId(Uuid::from_u128(1));

        manager.connected_modes.write().await.clear();
        manager.set_active(None, Some(no_images)).await;

        assert_eq!(*manager.desired_active.read().await, Some(no_images));
        assert!(no_images_rx.try_recv().is_err());
    }

    #[test]
    fn should_activate_on_connect_only_for_desired_mode() {
        let no_images = AutonomyModeId(Uuid::from_u128(1));
        let hive_mast = AutonomyModeId(Uuid::from_u128(2));

        assert!(should_activate_on_connect(no_images, Some(no_images)));
        assert!(!should_activate_on_connect(no_images, Some(hive_mast)));
        assert!(!should_activate_on_connect(no_images, None));
    }

    #[tokio::test]
    async fn shutdown_all_broadcasts_shutdown() {
        let (manager, mut no_images_rx, mut hive_rx, _out_tx) = mk_manager_with_channels();

        manager.shutdown_all().await;

        assert!(matches!(
            no_images_rx.recv().await,
            Some(AutonomyModeInput::Shutdown)
        ));
        assert!(matches!(
            hive_rx.recv().await,
            Some(AutonomyModeInput::Shutdown)
        ));
    }

    #[tokio::test]
    async fn try_recv_output_reads_queued_output() {
        let (mut manager, _no_images_rx, _hive_rx, out_tx) = mk_manager_with_channels();

        out_tx
            .send((
                AutonomyModeId(Uuid::from_u128(1)),
                AutonomyModeOutput::Fault("asdf".to_string()),
            ))
            .await
            .unwrap();

        let got = manager.try_recv_output();
        assert!(matches!(
            got,
            Some((AutonomyModeId(_), AutonomyModeOutput::Fault(_)))
        ));
    }

    #[tokio::test]
    async fn mode_statuses_marks_stale_heartbeat_unresponsive() {
        let (manager, _no_images_rx, _hive_rx, _out_tx) = mk_manager_with_channels();
        let mode = AutonomyModeId(Uuid::from_u128(1));
        manager.mode_statuses.write().await.insert(
            mode,
            ModeRuntimeStatus {
                connection: ModeConnectionState::Connected,
                last_heartbeat_unix_ms: Some(now_unix_ms().saturating_sub(15_001)),
                ..Default::default()
            },
        );

        let statuses = manager.mode_statuses().await;
        assert!(matches!(
            statuses[&mode].connection,
            ModeConnectionState::Unresponsive
        ));
    }

    #[tokio::test]
    async fn reconcile_configs_priority_change_does_not_restart_mode() {
        let (mut manager, mut no_images_rx, mut hive_rx, _out_tx) = mk_manager_with_channels();

        manager
            .reconcile_configs(vec![mk_config(1, 99), mk_config(2, 2)])
            .await
            .unwrap();

        assert!(no_images_rx.try_recv().is_err());
        assert!(hive_rx.try_recv().is_err());
        assert_eq!(
            manager.configs[&AutonomyModeId(Uuid::from_u128(1))].priority,
            1
        );
    }
}
