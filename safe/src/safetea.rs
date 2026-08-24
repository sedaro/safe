use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::{fs, time};
use tracing::{debug, error, info};
use uuid::Uuid;

use crate::RuntimePaths;
use crate::config::Config;
use crate::config_paths::{resolve_autonomy_mode_config_path, resolve_path_from_base};
use crate::definitions::Activation;
use crate::flight::{AutonomyModeActivation, Flight};
use crate::platform::gatekeeper::{
    GatekeeperAdapterInput, GatekeeperAdapterOutput, spawn_gatekeeper_adapter,
};
use crate::platform::{BoardPublicationStatus, spawn_platform_egress, spawn_platform_ingress};
use crate::router::AutonomyModeConfig;
use crate::router::Router;
use crate::runtime::{
    BoardCommandState, BoardCommandStatus, DaemonStatus, ModeOperationalStatus, OperationalStatus,
    TelemetryStatus, TelemetryStatusFrame,
};
use crate::sandbox::sandbox::SandboxResources;
use crate::telemetry_frame::TelemetryFrame;
use crate::utils::{append_jsonl, load_or_default_json, save_json_atomic};
use crate::{
    AutonomyModeId, AutonomyModeInput, AutonomyModeMeta, AutonomyModeOutput, BoardCmdId,
    BoardEvent, BoardState, Command, CommandEnvelope, ExternalCommand, HostCommandDispatchRecord,
    HostCommandRequest, HostCommandStatus, HostCommandStatusState, TimedCommand,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Msg {
    AutonomyModeCommandReceived(CommandEnvelope),
    ExecuteNow(Command),
    ExternalCommandReceived {
        request_id: Option<String>,
        command: ExternalCommand,
    },
    FaultRaised(String),
    GatekeeperApproved {
        id: BoardCmdId,
    },
    GatekeeperRejected {
        id: BoardCmdId,
        reason: String,
    },
    BoardCommandApproved {
        id: BoardCmdId,
        cmd: TimedCommand,
    },
    BoardCommandCanceled {
        id: BoardCmdId,
        reason: String,
        by: AutonomyModeId,
    },
    TelemetryReceived(TelemetryFrame),
    Tick,
}

impl Into<String> for &Msg {
    fn into(self) -> String {
        match self {
            Msg::TelemetryReceived(_) => "TelemetryReceived".to_string(),
            Msg::ExecuteNow(_) => "ExecuteNow".to_string(),
            Msg::ExternalCommandReceived { .. } => "ExternalCommandReceived".to_string(),
            Msg::AutonomyModeCommandReceived(_) => "AutonomyModeCommandReceived".to_string(),
            Msg::Tick => "Tick".to_string(),
            Msg::FaultRaised(_) => "FaultRaised".to_string(),
            Msg::GatekeeperApproved { .. } => "GatekeeperApproved".to_string(),
            Msg::GatekeeperRejected { .. } => "GatekeeperRejected".to_string(),
            Msg::BoardCommandApproved { .. } => "BoardCommandApproved".to_string(),
            Msg::BoardCommandCanceled { .. } => "BoardCommandCanceled".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Effect {
    Halt(String),
    ExecuteCommand(TimedCommand),
    Board(BoardEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Source {
    Telemetry,
    Controller,
    System,
    AutonomyMode,
    Gatekeeper,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub ts_mono: u64,
    pub source: Source,
    pub msg: Msg,
}

#[derive(Debug, Clone)]
struct PendingGatekeeperBatch {
    command_ids: Vec<BoardCmdId>,
}

pub(crate) fn update(flight: &mut Flight, ev: &Event) -> Vec<Effect> {
    let mut fx = Vec::new();

    match &ev.msg {
        Msg::TelemetryReceived(_) => {}

        Msg::ExecuteNow(c) => {
            fx.push(Effect::ExecuteCommand(TimedCommand::Now(c.clone())));
        }

        Msg::ExternalCommandReceived { command, .. } => {}

        Msg::AutonomyModeCommandReceived(env) => {
            let id = BoardCmdId::from_event(ev.seq, env.from, 0);
            fx.push(Effect::Board(BoardEvent::Proposed {
                id: id.clone(),
                from: env.from,
                cmd: env.cmd.clone(),
                ts_mono: ev.ts_mono,
            }));
        }

        Msg::GatekeeperApproved { id } => {
            fx.push(Effect::Board(BoardEvent::Approved {
                id: id.clone(),
                by: AutonomyModeId(Uuid::nil()),
                reason: "Gatekeeper approved command".to_string(),
                ts_mono: ev.ts_mono,
            }));
        }

        Msg::GatekeeperRejected { id, reason } => {
            fx.push(Effect::Board(BoardEvent::Canceled {
                id: id.clone(),
                by: AutonomyModeId(Uuid::nil()),
                reason: reason.clone(),
                ts_mono: ev.ts_mono,
            }));
        }

        Msg::BoardCommandApproved { id, cmd } => {
            fx.push(Effect::Board(BoardEvent::Approved {
                id: id.clone(),
                by: AutonomyModeId(Uuid::nil()),
                reason: "Board command approved".to_string(),
                ts_mono: ev.ts_mono,
            }));
        }

        Msg::BoardCommandCanceled { id, reason, by } => {
            fx.push(Effect::Board(BoardEvent::Canceled {
                id: id.clone(),
                by: *by,
                reason: reason.clone(),
                ts_mono: ev.ts_mono,
            }));
        }

        Msg::Tick => {}

        Msg::FaultRaised(reason) => {
            flight.set_fault(reason.clone());
            flight.stop();
            fx.push(Effect::Halt(reason.clone()));
        }
    }

    fx
}

pub(crate) async fn apply_event(
    flight: &mut Flight,
    ev: &Event,
    outputs_path: &PathBuf,
    emit_outputs: bool,
) -> anyhow::Result<Vec<Effect>> {
    if ev.seq <= flight.get_seq() {
        debug!(
            event_seq = ev.seq,
            last_seq_applied = flight.get_seq(),
            "dropping already-applied event"
        );
        return Ok(vec![]);
    }

    let effects = update(flight, ev);
    flight.set_seq(ev.seq);

    if emit_outputs {
        for fx in &effects {
            append_jsonl(outputs_path, fx).await?;
        }
    }

    Ok(effects)
}

pub async fn rebuild_board_from_outputs(
    runtime_paths: &RuntimePaths,
) -> anyhow::Result<BoardState> {
    let outputs_path = &runtime_paths.outputs;

    let mut board = BoardState::default();
    if !Path::new(outputs_path).exists() {
        return Ok(board);
    }

    let f = fs::File::open(outputs_path).await?;
    let mut lines = BufReader::new(f).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(&line) {
            Ok(fx) => {
                if let Effect::Board(bev) = fx {
                    board.apply(&bev);
                }
            }
            Err(_) => continue,
        }
    }
    Ok(board)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafeTEAAutonomyModeSummary {
    pub num_approved_commands: u128,
    pub num_rejected_commands: u128,
    pub last_approved_command_time: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafeTEASummary {
    pub num_telemetry_received: u128,
    pub autonomy_mode_summary: HashMap<AutonomyModeId, SafeTEAAutonomyModeSummary>,
    pub last_telemetry_time: Option<String>,
}

pub struct SafeTEA {
    board: BoardState,
    board_publication_rx: mpsc::Receiver<BoardPublicationStatus>,
    cfg: Config,
    config_reload_tick: time::Interval,
    external_command_rx: mpsc::Receiver<HostCommandRequest>,
    gatekeeper_input_tx: mpsc::Sender<GatekeeperAdapterInput>,
    gatekeeper_output_rx: mpsc::Receiver<GatekeeperAdapterOutput>,
    gatekeeper_next_request_id: u64,
    latest_gatekeeper_request_id: Option<u64>,
    pending_gatekeeper_batches: HashMap<u64, PendingGatekeeperBatch>,
    host_status_tx: mpsc::Sender<HostCommandStatus>,
    sent_board_command_ids: HashSet<BoardCmdId>,
    flight: Flight,
    host_command_dispatch_tx: mpsc::Sender<BoardState>,
    logical_ts: u64,
    latest_telemetry: Option<TelemetryFrame>,
    next_seq: u64,
    q: VecDeque<Event>,
    published_board_command_ids: HashSet<BoardCmdId>,
    router: Option<Router>,
    runtime_paths: RuntimePaths,
    telemetry_rx: mpsc::Receiver<TelemetryFrame>,
    tick: time::Interval,
    runtime_config_contents: String,
    status_tick: time::Interval,
    summary: SafeTEASummary,
    activation_clock_start: Instant,
}

fn default_true() -> bool {
    true
}

/// Used to configure a mode in the JSON autonomy modes configuration file
/// This is where you will tell SAFE where the autonomy mode executable is
/// on the filesystem, its priority, how many resources to give it, persist
/// the working directory, etc.
/// The `mode_config` is taken and passed down to the underlying mode and is
/// intended for mode-specific config, hence the generic type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AutonomyModeRuntimeConfig {
    name: String,
    priority: u8,
    #[serde(default = "default_true")]
    enabled: bool,
    bin_path: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    sandbox_resources: SandboxResources,
    #[serde(default)]
    persist_work_dir: bool,
    #[serde(default)]
    mode_config: serde_json::Value,
    #[serde(default)]
    activation: Option<Activation>,
}

impl AutonomyModeRuntimeConfig {
    /// Autonomy mode IDs are v5 UUIDs derived from the `AutonomyModeRuntimeConfig`
    /// name value.
    pub(crate) fn mode_id_from_name(name: &str) -> AutonomyModeId {
        Uuid::new_v5(&Uuid::NAMESPACE_OID, name.as_bytes()).into()
    }

    /// Loads the JSON autonomy modes configuration from the filesystem
    fn new(
        config_path: &Path,
        max_autonomy_modes: usize,
    ) -> anyhow::Result<(
        Vec<AutonomyModeConfig>,
        Vec<AutonomyModeMeta>,
        Vec<AutonomyModeActivation>,
        String,
    )> {
        let contents = std::fs::read_to_string(config_path)?;
        let config_dir = config_path.parent().unwrap_or(Path::new(""));
        let (runtime_mode_configs, mode_meta, mode_activations) =
            AutonomyModeRuntimeConfig::create_autonomy_mode_config_if_valid(
                &contents,
                max_autonomy_modes,
                Some(config_dir),
            )?;

        Ok((runtime_mode_configs, mode_meta, mode_activations, contents))
    }

    /// Loads the JSON autonomy modes configuration from the filesystem
    pub(crate) fn from_str(
        contents: &str,
        max_autonomy_modes: usize,
    ) -> anyhow::Result<(
        Vec<AutonomyModeConfig>,
        Vec<AutonomyModeMeta>,
        Vec<AutonomyModeActivation>,
    )> {
        Self::from_str_with_base(contents, max_autonomy_modes, None)
    }

    pub(crate) fn from_str_with_base(
        contents: &str,
        max_autonomy_modes: usize,
        config_base_dir: Option<&Path>,
    ) -> anyhow::Result<(
        Vec<AutonomyModeConfig>,
        Vec<AutonomyModeMeta>,
        Vec<AutonomyModeActivation>,
    )> {
        let (runtime_mode_configs, mode_meta, mode_activations) =
            AutonomyModeRuntimeConfig::create_autonomy_mode_config_if_valid(
                contents,
                max_autonomy_modes,
                config_base_dir,
            )?;

        Ok((runtime_mode_configs, mode_meta, mode_activations))
    }

    /// Checks to see whether configuration conditions align with invariants and
    /// creates the struct if so.
    fn create_autonomy_mode_config_if_valid(
        contents: &str,
        max_autonomy_modes: usize,
        config_base_dir: Option<&Path>,
    ) -> anyhow::Result<(
        Vec<AutonomyModeConfig>,
        Vec<AutonomyModeMeta>,
        Vec<AutonomyModeActivation>,
    )> {
        let runtime_mode_configs: Vec<AutonomyModeRuntimeConfig> = serde_json::from_str(contents)?;
        let mut seen_names = HashSet::new();

        let mut mode_configs: Vec<AutonomyModeConfig> = vec![];
        let mut mode_meta: Vec<AutonomyModeMeta> = vec![];
        let mut mode_activations: Vec<AutonomyModeActivation> = vec![];

        for config in runtime_mode_configs {
            if !seen_names.insert(config.name.clone()) {
                anyhow::bail!("Duplicate autonomy mode name found: {}", config.name);
            }
            if let Some(Activation::Timed { duration_secs, .. }) = &config.activation
                && *duration_secs == 0
            {
                anyhow::bail!(
                    "Timed activation for {} must have duration_secs > 0",
                    config.name
                );
            }
            info!("Autonomy mode: {config:?}");
            let id = AutonomyModeRuntimeConfig::mode_id_from_name(&config.name);
            mode_meta.push(AutonomyModeMeta {
                id,
                name: config.name.clone(),
                priority: config.priority,
                enabled: config.enabled,
            });
            mode_activations.push(AutonomyModeActivation {
                id,
                activation: config.activation.clone(),
            });
            if config.enabled {
                let bin_path = match config_base_dir {
                    Some(base) => resolve_path_from_base(base, &config.bin_path),
                    None => config.bin_path.clone(),
                };
                mode_configs.push(AutonomyModeConfig {
                    id,
                    priority: config.priority,
                    enabled: config.enabled,
                    bin_path,
                    args: config.args,
                    sandbox_resources: config.sandbox_resources,
                    persist_work_dir: config.persist_work_dir,
                    mode_config: config.mode_config,
                });
            }
        }

        if mode_meta.len() > max_autonomy_modes {
            anyhow::bail!(
                "Only {} are allowed per config, but {} were found.",
                max_autonomy_modes,
                mode_meta.len()
            );
        }

        Ok((mode_configs, mode_meta, mode_activations))
    }
}

impl SafeTEA {
    pub async fn recover_from_log_with_autonomy_mode_delivery(&mut self) -> anyhow::Result<()> {
        let events_path = &self.runtime_paths.events;

        if !Path::new(events_path).exists() {
            return Ok(());
        }

        let f = fs::File::open(events_path).await?;
        let mut lines = BufReader::new(f).lines();

        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }

            match serde_json::from_str::<Event>(&line) {
                Ok(ev) => {
                    if ev.seq <= self.flight.get_seq() {
                        continue;
                    }

                    self.apply_event(&ev, false).await?;

                    match &ev.msg {
                        Msg::TelemetryReceived(t) => {
                            self.flight.note_telemetry(t);
                            self.latest_telemetry = Some(t.clone());
                            let previous_active = self.flight.get_active_autonomy_mode();
                            self.flight
                                .recalculate_active_autonomy_mode_at(self.activation_now_ms());
                            if let Some(router) = self.router.as_mut() {
                                router
                                    .set_active(
                                        previous_active,
                                        self.flight.get_active_autonomy_mode(),
                                    )
                                    .await;
                            }

                            if let Some(router) = self.router.as_mut() {
                                router.send_telemetry_to_all(t.clone()).await;
                            }
                        }
                        _ => {}
                    }
                }
                Err(_) => {
                    debug!("skipping unparsable line in events log during recovery");
                    continue;
                }
            }
        }

        Ok(())
    }

    pub async fn rehydrate_autonomy_mode_runtime(&mut self) -> anyhow::Result<()> {
        if let Some(active) = self.flight.get_active_autonomy_mode() {
            if let Some(router) = self.router.as_mut() {
                info!("Desired active mode: {active:?}");
                router.set_active(None, Some(active)).await;
            }
        }
        Ok(())
    }

    pub async fn new(cfg: Config, runtime_paths: RuntimePaths) -> Self {
        // Flight JSON contains state from last run which is key for power cycles
        let mut flight = load_or_default_json(&runtime_paths.flight, Flight::default())
            .await
            .expect("json load");

        let mut summary = load_or_default_json(
            &runtime_paths.summary,
            SafeTEASummary {
                num_telemetry_received: 0,
                autonomy_mode_summary: HashMap::new(),
                last_telemetry_time: None,
            },
        )
        .await
        .expect("json load");

        let autonomy_mode_config_path = resolve_autonomy_mode_config_path();
        info!(
            "Autonomy mode config: {}",
            autonomy_mode_config_path.display()
        );
        let (mode_configs, mode_meta, mode_activations, runtime_config_contents) =
            AutonomyModeRuntimeConfig::new(
                &autonomy_mode_config_path,
                cfg.limits.max_autonomy_modes,
            )
            .expect("config load");

        flight.set_autonomy_modes(mode_meta);
        flight.set_autonomy_mode_activations(mode_activations);
        flight.recalculate_active_autonomy_mode();

        // Bootstrap router from mode configs at runtime
        let router = if mode_configs.is_empty() {
            None
        } else {
            Some(
                Router::start(mode_configs, &runtime_paths)
                    .await
                    .expect("router start"),
            )
        };

        let board = rebuild_board_from_outputs(&runtime_paths)
            .await
            .expect("rebuild board");
        let sent_board_command_ids: HashSet<BoardCmdId> =
            board.source_of_truth.iter().cloned().collect();

        let (telemetry_tx, telemetry_rx) = mpsc::channel::<TelemetryFrame>(1024);
        let (external_command_tx, external_command_rx) = mpsc::channel::<HostCommandRequest>(1024);
        let (host_status_tx, host_status_rx) = mpsc::channel::<HostCommandStatus>(1024);
        let (host_command_dispatch_tx, host_command_dispatch_rx) =
            mpsc::channel::<BoardState>(1024);
        let (board_publication_tx, board_publication_rx) =
            mpsc::channel::<BoardPublicationStatus>(1024);
        spawn_platform_ingress(
            &cfg,
            &runtime_paths,
            telemetry_tx.clone(),
            external_command_tx.clone(),
        )
        .expect("platform ingress");
        spawn_platform_egress(
            &cfg,
            &runtime_paths,
            host_status_rx,
            host_command_dispatch_rx,
            board_publication_tx,
        )
        .expect("platform egress");

        let (gatekeeper_input_tx, gatekeeper_output_rx) =
            spawn_gatekeeper_adapter(&cfg).expect("gatekeeper adapter");

        let next_seq = flight.peak_next_seq();
        let mut safetea = Self {
            board,
            board_publication_rx,
            cfg,
            config_reload_tick: time::interval(Duration::from_secs(1)),
            external_command_rx,
            gatekeeper_input_tx,
            gatekeeper_output_rx,
            gatekeeper_next_request_id: 1,
            latest_gatekeeper_request_id: None,
            pending_gatekeeper_batches: HashMap::new(),
            sent_board_command_ids,
            flight,
            host_command_dispatch_tx,
            host_status_tx,
            logical_ts: next_seq,
            latest_telemetry: None,
            next_seq,
            q: VecDeque::new(),
            published_board_command_ids: HashSet::new(),
            router,
            runtime_paths,
            telemetry_rx,
            tick: time::interval(Duration::from_millis(100)),
            runtime_config_contents,
            status_tick: time::interval(Duration::from_secs(1)),
            summary,
            activation_clock_start: Instant::now(),
        };

        safetea
            .rehydrate_autonomy_mode_runtime()
            .await
            .expect("rehydrate");

        safetea
            .recover_from_log_with_autonomy_mode_delivery()
            .await
            .expect("recover");

        let recovered_next_seq = safetea.flight.peak_next_seq();
        if safetea.next_seq != recovered_next_seq {
            info!(
                previous_next_seq = safetea.next_seq,
                recovered_next_seq, "resyncing event sequence counter after recovery"
            );
            safetea.next_seq = recovered_next_seq;
        }
        if safetea.logical_ts < recovered_next_seq {
            safetea.logical_ts = recovered_next_seq;
        }

        save_json_atomic(&safetea.runtime_paths.flight, &safetea.flight)
            .await
            .expect("atomic save");

        safetea.send_board_snapshot_to_all().await;
        safetea
            .write_operational_status()
            .await
            .expect("write initial status");

        safetea
    }

    fn activation_now_ms(&self) -> u64 {
        self.activation_clock_start.elapsed().as_millis() as u64
    }

    pub async fn send_board_snapshot_to_all(&mut self) {
        if let Some(router) = self.router.as_ref() {
            router.send_board_snapshot_to_all(self.board.clone()).await;
        }
    }

    fn board_statuses(&self) -> Vec<BoardCommandStatus> {
        let mut entries: Vec<BoardCommandStatus> = self
            .board
            .proposals
            .iter()
            .map(|(id, (from, command, proposed_ts_mono))| {
                let rejected = self
                    .board
                    .rejected
                    .get(id)
                    .and_then(|entries| entries.last());
                let approved = self
                    .board
                    .approved
                    .get(id)
                    .and_then(|entries| entries.last());
                let (state, decision) = if let Some(decision) = rejected {
                    (BoardCommandState::Rejected, Some(decision))
                } else if self.published_board_command_ids.contains(id) {
                    (BoardCommandState::Published, approved)
                } else if let Some(decision) = approved {
                    (BoardCommandState::Approved, Some(decision))
                } else {
                    (BoardCommandState::Pending, None)
                };

                let (decision_by, decision_reason, decision_ts_mono) = decision
                    .map(|(by, reason, ts_mono)| {
                        let by = if by.0.is_nil() {
                            "gatekeeper".to_string()
                        } else {
                            by.to_string()
                        };
                        (Some(by), Some(reason.clone()), Some(*ts_mono))
                    })
                    .unwrap_or((None, None, None));

                BoardCommandStatus {
                    id: id.clone(),
                    from: *from,
                    command: command.clone(),
                    proposed_ts_mono: *proposed_ts_mono,
                    state,
                    decision_by,
                    decision_reason,
                    decision_ts_mono,
                }
            })
            .collect();
        entries.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        entries
    }

    async fn write_operational_status(&self) -> anyhow::Result<()> {
        let router_statuses = match self.router.as_ref() {
            Some(router) => router.mode_statuses().await,
            None => HashMap::new(),
        };
        let active = self.flight.get_active_autonomy_mode();
        let selection_reason = self.flight.active_selection_reason();
        let manual_override = self.flight.get_manual_active_override();
        let mut modes: Vec<ModeOperationalStatus> = self
            .flight
            .get_autonomy_modes()
            .iter()
            .map(|meta| {
                let (eligible, eligibility_reason) = self.flight.mode_eligibility(meta.id);
                let is_active = active == Some(meta.id);
                ModeOperationalStatus {
                    name: meta.name.clone(),
                    id: meta.id,
                    priority: meta.priority,
                    enabled: meta.enabled,
                    eligible,
                    active: is_active,
                    manual_override: manual_override == Some(meta.id),
                    selection_reason: if is_active {
                        selection_reason.clone()
                    } else {
                        "not selected".to_string()
                    },
                    eligibility_reason,
                    runtime: router_statuses.get(&meta.id).cloned().unwrap_or_default(),
                }
            })
            .collect();
        modes.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.name.cmp(&b.name))
        });

        let status = OperationalStatus {
            schema_version: 1,
            updated_at: Utc::now().to_rfc3339(),
            daemon: DaemonStatus {
                pid: std::process::id(),
                running: self.flight.is_running(),
                halted: self.flight.is_halted(),
                fault: self.flight.get_fault().clone(),
                last_seq_applied: self.flight.get_seq(),
            },
            telemetry: TelemetryStatus {
                received_count: self.summary.num_telemetry_received,
                last_received_at: self.summary.last_telemetry_time.clone(),
                latest: self
                    .latest_telemetry
                    .as_ref()
                    .map(|telemetry| TelemetryStatusFrame {
                        source: telemetry.source.clone(),
                        ts_mono: telemetry.ts_mono,
                        payload: telemetry.payload.clone(),
                    }),
            },
            board: self.board_statuses(),
            modes,
        };
        save_json_atomic(&self.runtime_paths.status, &status).await
    }

    fn collect_unapproved_board_batch(&self) -> Option<(Vec<BoardCmdId>, Vec<TimedCommand>)> {
        let mut items: Vec<(BoardCmdId, TimedCommand)> = self
            .board
            .proposals
            .iter()
            .filter_map(|(id, (_from, cmd, _))| {
                if self.sent_board_command_ids.contains(id) {
                    return None;
                }

                let is_approved = self
                    .board
                    .approved
                    .get(id)
                    .map(|v| !v.is_empty())
                    .unwrap_or(false);
                if is_approved {
                    return None;
                }

                let is_rejected = self
                    .board
                    .rejected
                    .get(id)
                    .map(|v| !v.is_empty())
                    .unwrap_or(false);
                if is_rejected {
                    return None;
                }

                Some((id.clone(), cmd.clone()))
            })
            .collect();

        if items.is_empty() {
            return None;
        }

        items.sort_by(|a, b| a.0.0.cmp(&b.0.0));

        let (command_ids, commands) = items.into_iter().unzip();

        Some((command_ids, commands))
    }

    fn has_pending_autonomy_mode_command_events(&self) -> bool {
        self.q
            .iter()
            .any(|event| matches!(event.msg, Msg::AutonomyModeCommandReceived(_)))
    }

    async fn request_gatekeeper_for_pending_batch(&mut self) {
        let Some((command_ids, _commands)) = self.collect_unapproved_board_batch() else {
            self.latest_gatekeeper_request_id = None;
            self.pending_gatekeeper_batches.clear();
            return;
        };

        if let Some(request_id) = self.latest_gatekeeper_request_id {
            debug!(
                request_id,
                unsent_commands = command_ids.len(),
                "gatekeeper evaluation already in-flight; deferring"
            );
            return;
        }

        let request_id = self.gatekeeper_next_request_id;
        self.gatekeeper_next_request_id += 1;

        self.pending_gatekeeper_batches.clear();
        self.pending_gatekeeper_batches.insert(
            request_id,
            PendingGatekeeperBatch {
                command_ids: command_ids.clone(),
            },
        );
        self.latest_gatekeeper_request_id = Some(request_id);

        let batch_size = command_ids.len();
        info!(request_id, batch_size, "requesting gatekeeper evaluation");

        self.gatekeeper_input_tx
            .send(GatekeeperAdapterInput::EvaluateBatch {
                request_id,
                board: self.board.clone(),
                candidate_command_ids: command_ids,
            })
            .await
            .expect("gatekeeper input");
    }

    pub async fn run(&mut self) {
        let autonomy_mode_config_path = resolve_autonomy_mode_config_path();

        loop {
            tokio::select! {
                // Received telemetry from socket
                maybe_t = self.telemetry_rx.recv() => {
                    if let Some(t) = maybe_t {
                        self.q.push_back(Event {
                            seq: self.next_seq,
                            ts_mono: t.ts_mono,
                            source: Source::Telemetry,
                            msg: Msg::TelemetryReceived(t),
                        });
                        self.summary.num_telemetry_received += 1;
                        self.summary.last_telemetry_time = Some(Utc::now().to_rfc3339());
                        self.next_seq += 1;
                    }
                }

                maybe_publication = self.board_publication_rx.recv() => {
                    if let Some(publication) = maybe_publication {
                        self.published_board_command_ids
                            .extend(publication.command_ids);
                        if let Err(e) = self.write_operational_status().await {
                            error!("failed writing operational status: {e}");
                        }
                    }
                }

                // Received command from safectl
                maybe_ec = self.external_command_rx.recv() => {
                    if let Some(c) = maybe_ec {
                        let _ = self.host_status_tx.send(HostCommandStatus {
                            request_id: c.request_id.clone(),
                            state: HostCommandStatusState::Received,
                            detail: "command received".to_string(),
                            ts_mono: self.logical_ts,
                        }).await;

                        self.q.push_back(Event {
                            seq: self.next_seq,
                            ts_mono: self.logical_ts,
                            source: Source::Controller,
                            msg: Msg::ExternalCommandReceived {
                                request_id: Some(c.request_id),
                                command: c.command,
                            },
                        });
                        self.next_seq += 1;
                        self.logical_ts += 1;
                    }
                }

                maybe_gk = self.gatekeeper_output_rx.recv() => {
                    if let Some(gk_out) = maybe_gk {
                        match gk_out {
                            GatekeeperAdapterOutput::Approve { request_id, details } => {
                                let Some(batch) = self.pending_gatekeeper_batches.remove(&request_id) else {
                                    info!("ignoring gatekeeper approval for unknown request_id={request_id}");
                                    continue;
                                };

                                if self.latest_gatekeeper_request_id != Some(request_id) {
                                    info!("ignoring stale gatekeeper approval request_id={request_id}");
                                    continue;
                                }

                                self.pending_gatekeeper_batches.clear();
                                self.latest_gatekeeper_request_id = None;
                                self.next_seq += 1;
                                self.logical_ts += 1;

                                for id in batch.command_ids {
                                    self.sent_board_command_ids.insert(id.clone());
                                    self.q.push_back(Event {
                                        seq: self.next_seq,
                                        ts_mono: self.logical_ts,
                                        source: Source::Gatekeeper,
                                        msg: Msg::GatekeeperApproved { id: id.clone() },
                                    });

                                    match id.parse() {
                                        Some((_seq, autonomy_mode_id, _local_idx)) => {
                                            let summary = self
                                                .summary
                                                .autonomy_mode_summary
                                                .entry(autonomy_mode_id)
                                                .or_insert(SafeTEAAutonomyModeSummary {
                                                    num_approved_commands: 0,
                                                    num_rejected_commands: 0,
                                                    last_approved_command_time: None,
                                                });
                                            summary.num_approved_commands += 1;
                                            summary.last_approved_command_time =
                                                Some(Utc::now().to_rfc3339());
                                        }
                                        None => {
                                            info!(
                                                request_id,
                                                "approved command with unparseable id, details from gatekeeper: {details}"
                                            );
                                        }
                                    }

                                    self.next_seq += 1;
                                    self.logical_ts += 1;
                                }

                                self.request_gatekeeper_for_pending_batch().await;
                            }
                            GatekeeperAdapterOutput::Reject { request_id, reason } => {
                                let Some(batch) = self.pending_gatekeeper_batches.remove(&request_id) else {
                                    info!("ignoring gatekeeper rejection for unknown request_id={request_id}");
                                    continue;
                                };

                                if self.latest_gatekeeper_request_id != Some(request_id) {
                                    info!("ignoring stale gatekeeper rejection request_id={request_id}");
                                    continue;
                                }

                                self.pending_gatekeeper_batches.clear();
                                self.latest_gatekeeper_request_id = None;
                                self.next_seq += 1;
                                self.logical_ts += 1;

                                for id in batch.command_ids {
                                    self.sent_board_command_ids.insert(id.clone());
                                    self.q.push_back(Event {
                                        seq: self.next_seq,
                                        ts_mono: self.logical_ts,
                                        source: Source::Gatekeeper,
                                        msg: Msg::GatekeeperRejected {
                                            id: id.clone(),
                                            reason: reason.clone(),
                                        },
                                    });

                                    match id.parse() {
                                        Some((_seq, autonomy_mode_id, _local_idx)) => {
                                            let summary = self
                                                .summary
                                                .autonomy_mode_summary
                                                .entry(autonomy_mode_id)
                                                .or_insert(SafeTEAAutonomyModeSummary {
                                                    num_approved_commands: 0,
                                                    num_rejected_commands: 0,
                                                    last_approved_command_time: None,
                                                });
                                            summary.num_rejected_commands += 1;
                                        }
                                        None => {
                                            info!(
                                                request_id,
                                                "rejected command with unparseable id, details from gatekeeper: {reason}"
                                            );
                                        }
                                    }

                                    self.next_seq += 1;
                                    self.logical_ts += 1;
                                }

                                self.request_gatekeeper_for_pending_batch().await;
                            }
                        }
                    }
                }

                // Tick every 100ms
                _ = self.tick.tick() => {
                    self.q.push_back(Event {
                        seq: self.next_seq,
                        ts_mono: self.logical_ts,
                        source: Source::System,
                        msg: Msg::Tick,
                    });
                    self.next_seq += 1;
                    self.logical_ts += 1;
                }

                _ = self.status_tick.tick() => {
                    if let Err(e) = self.write_operational_status().await {
                        error!("failed writing operational status: {e}");
                    }
                }

                // Reload autonomy mode config at runtime mid-flight
                _ = self.config_reload_tick.tick() => {
                    match fs::read_to_string(&autonomy_mode_config_path).await {
                        Ok(new_contents) if new_contents != self.runtime_config_contents => {
                            info!("Detected autonomy mode config change; reloading modes");
                            let config_base_dir = autonomy_mode_config_path
                                .parent()
                                .unwrap_or(Path::new(""));
                            match AutonomyModeRuntimeConfig::from_str_with_base(
                                &new_contents,
                                self.cfg.limits.max_autonomy_modes,
                                Some(config_base_dir),
                            ) {
                                Ok((new_configs, new_meta, new_activations)) => {
                                    match self.router.as_mut() {
                                        Some(router) => {
                                            if let Err(e) = router.reconcile_configs(new_configs).await {
                                                error!("Failed to reconcile reloaded autonomy modes: {e}");
                                                continue;
                                            }
                                        }
                                        None => {
                                            if !new_configs.is_empty() {
                                                match Router::start(new_configs, &self.runtime_paths).await {
                                                    Ok(router) => self.router = Some(router),
                                                    Err(e) => {
                                                        error!("Failed to start reloaded autonomy modes: {e}");
                                                        continue;
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    self.flight.set_autonomy_modes(new_meta);
                                    self.flight.set_autonomy_mode_activations(new_activations);
                                    self.flight.recalculate_active_autonomy_mode_at(self.activation_now_ms());
                                    if let Some(router) = self.router.as_ref() {
                                        router.set_active(None, self.flight.get_active_autonomy_mode()).await;
                                        router.send_board_snapshot_to_all(self.board.clone()).await;
                                    }
                                    self.runtime_config_contents = new_contents;
                                    save_json_atomic(&self.runtime_paths.flight, &self.flight).await.expect("json save");
                                    save_json_atomic(&self.runtime_paths.summary, &self.summary).await.expect("json save");
                                }
                                Err(e) => {
                                    error!("Failed to reload autonomy mode config: {e}");
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            error!("Failed reading autonomy mode config during reload: {e}");
                        }
                    }
                }
            }

            if let Some(router) = self.router.as_mut() {
                while let Some((pid, out)) = router.try_recv_output() {
                    match out {
                        AutonomyModeOutput::CancelBoard { id, reason } => {
                            match id.parse() {
                                Some((_seq, autonomy_mode_id, _local_idx)) => {
                                    let summary = self
                                        .summary
                                        .autonomy_mode_summary
                                        .entry(autonomy_mode_id)
                                        .or_insert(SafeTEAAutonomyModeSummary {
                                            num_approved_commands: 0,
                                            num_rejected_commands: 0,
                                            last_approved_command_time: None,
                                        });
                                    summary.num_rejected_commands += 1;
                                }
                                None => {
                                    info!("rejected command with unparseable id={id:?}");
                                }
                            }

                            self.q.push_back(Event {
                                seq: self.next_seq,
                                ts_mono: self.logical_ts,
                                source: Source::AutonomyMode,
                                msg: Msg::BoardCommandCanceled {
                                    id,
                                    reason,
                                    by: pid,
                                },
                            });

                            self.next_seq += 1;
                            self.logical_ts += 1;
                        }
                        AutonomyModeOutput::Command(env) => {
                            self.flight.set_last_planned_autonomy_mode(env.from);

                            self.q.push_back(Event {
                                seq: self.next_seq,
                                ts_mono: self.logical_ts,
                                source: Source::AutonomyMode,
                                msg: Msg::AutonomyModeCommandReceived(env),
                            });

                            self.next_seq += 1;
                            self.logical_ts += 1;
                        }
                        AutonomyModeOutput::Fault(reason) => {
                            error!(
                                mode = %pid,
                                reason = %reason,
                                "autonomy mode reported fault"
                            );
                            self.q.push_back(Event {
                                seq: self.next_seq,
                                ts_mono: self.logical_ts,
                                source: Source::AutonomyMode,
                                msg: Msg::FaultRaised(format!("{pid:?}: {reason}")),
                            });
                            self.next_seq += 1;
                            self.logical_ts += 1;
                        }
                        AutonomyModeOutput::Lifecycle { .. } | AutonomyModeOutput::Heartbeat => {}
                    }
                }
            }

            if let Some(ev) = self.q.pop_front() {
                // To keep the event log compact
                match &ev.msg {
                    Msg::Tick => {}
                    _ => {
                        append_jsonl(&self.runtime_paths.events, &ev)
                            .await
                            .expect("event append");
                    }
                }

                let effects = self.apply_event(&ev, true).await.expect("apply event");

                let mut board_changed = false;
                for fx in &effects {
                    if let Effect::ExecuteCommand(cmd) = fx {
                        // if let Err(e) = self
                        //     .host_command_dispatch_tx
                        //     .send(HostCommandDispatchRecord {
                        //         event_seq: ev.seq,
                        //         event_ts_mono: ev.ts_mono,
                        //         event_source: format!("{:?}", ev.source),
                        //         event_msg_kind: (&ev.msg).into(),
                        //         timed_command: cmd.clone(),
                        //     })
                        //     .await
                        // {
                        //     error!("failed to enqueue host command dispatch record: {e}");
                        // }
                    }
                    if let Effect::Board(bev) = fx {
                        self.board.apply(bev);
                        board_changed = true;
                    }
                }

                if let Err(e) = self.host_command_dispatch_tx.send(self.board.clone()).await {
                    error!("failed to enqueue host command dispatch record: {e}");
                }

                if board_changed {
                    if let Some(router) = self.router.as_ref() {
                        router.send_board_snapshot_to_all(self.board.clone()).await;
                    }

                    if !self.has_pending_autonomy_mode_command_events() {
                        self.request_gatekeeper_for_pending_batch().await;
                    }
                }

                if let Msg::TelemetryReceived(t) = &ev.msg {
                    self.flight.note_telemetry(t);
                    self.latest_telemetry = Some(t.clone());
                    let previous_active = self
                        .flight
                        .recalculate_active_autonomy_mode_at(self.activation_now_ms());
                    if let Some(router) = self.router.as_ref() {
                        router
                            .set_active(previous_active, self.flight.get_active_autonomy_mode())
                            .await;
                    }

                    if let Some(router) = self.router.as_ref() {
                        router.send_telemetry_to_all(t.clone()).await;
                    }
                    let _ = self
                        .gatekeeper_input_tx
                        .send(GatekeeperAdapterInput::Telemetry(t.clone()))
                        .await;
                }

                if let Msg::ExternalCommandReceived {
                    request_id,
                    command,
                } = &ev.msg
                {
                    if let Some(request_id) = request_id.clone() {
                        let _ = self
                            .host_status_tx
                            .send(HostCommandStatus {
                                request_id,
                                state: HostCommandStatusState::Accepted,
                                detail: "command accepted".to_string(),
                                ts_mono: ev.ts_mono,
                            })
                            .await;
                    }

                    if let Some(router) = self.router.as_ref() {
                        match command {
                            ExternalCommand::ExecuteNow { command } => {
                                if let Some(request_id) = request_id.clone() {
                                    let _ = self
                                        .host_status_tx
                                        .send(HostCommandStatus {
                                            request_id,
                                            state: HostCommandStatusState::Dispatched,
                                            detail: "execute_now dispatched".to_string(),
                                            ts_mono: ev.ts_mono,
                                        })
                                        .await;
                                }

                                self.q.push_back(Event {
                                    seq: self.next_seq,
                                    ts_mono: self.logical_ts,
                                    source: Source::Controller,
                                    msg: Msg::ExecuteNow(command.clone()),
                                });
                                self.next_seq += 1;
                                self.logical_ts += 1;
                            }
                            ExternalCommand::ActivateMode { mode } => {
                                if router
                                    .send_input_to(*mode, AutonomyModeInput::Activate)
                                    .await
                                {
                                    self.flight.set_manual_active_override(*mode);
                                    self.flight.set_active_autonomy_mode(*mode);
                                    router.set_desired_active(Some(*mode)).await;
                                    info!("external command: activated mode {mode}");
                                    if let Some(request_id) = request_id.clone() {
                                        let _ = self
                                            .host_status_tx
                                            .send(HostCommandStatus {
                                                request_id,
                                                state: HostCommandStatusState::Dispatched,
                                                detail: format!("activated mode {mode}"),
                                                ts_mono: ev.ts_mono,
                                            })
                                            .await;
                                    }
                                } else {
                                    error!("external command: mode not found for activate {mode}");
                                    if let Some(request_id) = request_id.clone() {
                                        let _ = self
                                            .host_status_tx
                                            .send(HostCommandStatus {
                                                request_id,
                                                state: HostCommandStatusState::Failed,
                                                detail: format!(
                                                    "mode not found for activate {mode}"
                                                ),
                                                ts_mono: ev.ts_mono,
                                            })
                                            .await;
                                    }
                                }
                            }
                            ExternalCommand::DeactivateMode { mode } => {
                                if router
                                    .send_input_to(*mode, AutonomyModeInput::Deactivate)
                                    .await
                                {
                                    if self.flight.get_manual_active_override() == Some(*mode) {
                                        self.flight.clear_manual_active_override();
                                    }
                                    if self.flight.is_active_autonomy_mode(Some(*mode)) {
                                        self.flight.clear_active_autonomy_mode();
                                    }
                                    let previous = self.flight.get_active_autonomy_mode();
                                    self.flight.recalculate_active_autonomy_mode_at(
                                        self.activation_now_ms(),
                                    );
                                    router
                                        .set_active(
                                            previous,
                                            self.flight.get_active_autonomy_mode(),
                                        )
                                        .await;
                                    info!("external command: deactivated mode {mode}");
                                    if let Some(request_id) = request_id.clone() {
                                        let _ = self
                                            .host_status_tx
                                            .send(HostCommandStatus {
                                                request_id,
                                                state: HostCommandStatusState::Dispatched,
                                                detail: format!("deactivated mode {mode}"),
                                                ts_mono: ev.ts_mono,
                                            })
                                            .await;
                                    }
                                } else {
                                    error!(
                                        "external command: mode not found for deactivate {mode}"
                                    );
                                    if let Some(request_id) = request_id.clone() {
                                        let _ = self
                                            .host_status_tx
                                            .send(HostCommandStatus {
                                                request_id,
                                                state: HostCommandStatusState::Failed,
                                                detail: format!(
                                                    "mode not found for deactivate {mode}"
                                                ),
                                                ts_mono: ev.ts_mono,
                                            })
                                            .await;
                                    }
                                }
                            }
                            ExternalCommand::StopMode { mode } => {
                                if router
                                    .send_input_to(*mode, AutonomyModeInput::Shutdown)
                                    .await
                                {
                                    if self.flight.get_manual_active_override() == Some(*mode) {
                                        self.flight.clear_manual_active_override();
                                    }
                                    if self.flight.is_active_autonomy_mode(Some(*mode)) {
                                        self.flight.clear_active_autonomy_mode();
                                    }
                                    let previous = self.flight.get_active_autonomy_mode();
                                    self.flight.recalculate_active_autonomy_mode_at(
                                        self.activation_now_ms(),
                                    );
                                    router
                                        .set_active(
                                            previous,
                                            self.flight.get_active_autonomy_mode(),
                                        )
                                        .await;
                                    info!("external command: stopped mode {mode}");
                                    if let Some(request_id) = request_id.clone() {
                                        let _ = self
                                            .host_status_tx
                                            .send(HostCommandStatus {
                                                request_id,
                                                state: HostCommandStatusState::Dispatched,
                                                detail: format!("stopped mode {mode}"),
                                                ts_mono: ev.ts_mono,
                                            })
                                            .await;
                                    }
                                } else {
                                    error!("external command: mode not found for stop {mode}");
                                    if let Some(request_id) = request_id.clone() {
                                        let _ = self
                                            .host_status_tx
                                            .send(HostCommandStatus {
                                                request_id,
                                                state: HostCommandStatusState::Failed,
                                                detail: format!("mode not found for stop {mode}"),
                                                ts_mono: ev.ts_mono,
                                            })
                                            .await;
                                    }
                                }
                            }
                            ExternalCommand::RestartMode { mode } => {
                                if router
                                    .send_input_to(*mode, AutonomyModeInput::Restart)
                                    .await
                                {
                                    self.flight.set_manual_active_override(*mode);
                                    router.set_desired_active(Some(*mode)).await;
                                    self.flight.set_active_autonomy_mode(*mode);
                                    info!("external command: restart requested for mode {mode}");
                                    if let Some(request_id) = request_id.clone() {
                                        let _ = self
                                            .host_status_tx
                                            .send(HostCommandStatus {
                                                request_id,
                                                state: HostCommandStatusState::Dispatched,
                                                detail: format!(
                                                    "restart requested for mode {mode}"
                                                ),
                                                ts_mono: ev.ts_mono,
                                            })
                                            .await;
                                    }
                                } else {
                                    error!("external command: mode not found for restart {mode}");
                                    if let Some(request_id) = request_id.clone() {
                                        let _ = self
                                            .host_status_tx
                                            .send(HostCommandStatus {
                                                request_id,
                                                state: HostCommandStatusState::Failed,
                                                detail: format!(
                                                    "mode not found for restart {mode}"
                                                ),
                                                ts_mono: ev.ts_mono,
                                            })
                                            .await;
                                    }
                                }
                            }
                        }
                    }
                }

                save_json_atomic(&self.runtime_paths.flight, &self.flight)
                    .await
                    .expect("save json");
                save_json_atomic(&self.runtime_paths.summary, &self.summary)
                    .await
                    .expect("save json");

                if !matches!(ev.msg, Msg::Tick)
                    && let Err(e) = self.write_operational_status().await
                {
                    error!("failed writing operational status: {e}");
                }

                if self.flight.is_halted() {
                    break;
                }
            }
        }

        if let Some(router) = self.router.as_ref() {
            router.shutdown_all().await;
        }
    }

    async fn apply_event(&mut self, ev: &Event, emit_outputs: bool) -> anyhow::Result<Vec<Effect>> {
        apply_event(
            &mut self.flight,
            ev,
            &self.runtime_paths.outputs,
            emit_outputs,
        )
        .await
    }

    fn update(&mut self, ev: &Event) -> Vec<Effect> {
        update(&mut self.flight, ev)
    }
}
