use std::collections::{HashMap, VecDeque};
use std::env::var;
use std::io;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use chrono::Utc;
use clap::{ArgAction, CommandFactory, Parser, Subcommand, ValueEnum};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event as CEvent, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{
    Block, Borders, Cell as TuiCell, Paragraph, Row as TuiRow, Table as TuiTable,
};
use safe::protocol::Command;
use safe::runtime::{
    AutonomyModeConfigItem, BoardCommandState, BoardCommandStatus, ExternalCommand,
    FlightCheckpoint, HostCommandStatus, ModeConnectionState, ModeHandlerState,
    ModeOperationalStatus, ModeResourceSnapshot, OperationalStatus, RuntimeConfigView,
    SafectlIngress, mode_id_from_name,
};
use safe::telemetry_frame::TelemetryFrame;
use serde_json::Value;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command as TokioCommand;
use tokio::time::{self, Duration};
use uuid::Uuid;

mod logs;
mod output;

use logs::{LogOutputFormat, run_logs};
use output::{
    process_tree_rows, render_board_table, render_mode_describe_table, render_modes_table,
    render_request_table, render_status_table, render_telemetry_table, top_headers, top_row_values,
};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[arg(short, long, short_alias = 'v', action = ArgAction::Count)]
    debug: u8,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    Table,
    Json,
}

#[derive(Subcommand)]
enum GetObject {
    Modes {
        #[arg(short = 'A', long)]
        all: bool,

        #[arg()]
        name: Option<String>,

        #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
        output: OutputFormat,
    },
    Telemetry {
        #[arg(short = 'n', long, default_value_t = 1)]
        tail: usize,

        #[arg(long)]
        source: Option<String>,

        #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
        output: OutputFormat,
    },
    Board {
        #[arg(long, value_enum)]
        state: Option<BoardStateFilter>,

        #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
        output: OutputFormat,
    },
    Request {
        #[arg()]
        request_id: String,

        #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
        output: OutputFormat,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum BoardStateFilter {
    Pending,
    Approved,
    Rejected,
    Published,
}

#[derive(Subcommand)]
enum DescribeObject {
    Mode {
        #[arg()]
        name: String,

        #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
        output: OutputFormat,
    },
}

#[derive(Subcommand)]
enum Commands {
    Get {
        #[command(subcommand)]
        command: GetObject,
    },
    Describe {
        #[command(subcommand)]
        command: DescribeObject,
    },
    Logs {
        #[arg()]
        mode: Option<String>,

        #[arg(short = 'm', long)]
        mode_name: Option<String>,

        #[arg(long)]
        id: Option<Uuid>,

        #[arg(short = 'A', long)]
        all_modes: bool,

        #[arg(short = 'n', long, default_value_t = 100)]
        tail: usize,

        #[arg(short, long, default_value_t = false)]
        follow: bool,

        #[arg(long)]
        since: Option<String>,

        #[arg(long)]
        before: Option<String>,

        #[arg(long)]
        filter: Option<String>,

        #[arg(short = 'l', long)]
        level: Option<String>,

        #[arg(short = 'o', long, value_enum, default_value_t = LogOutputFormat::Text)]
        output: LogOutputFormat,
    },
    Top {
        #[command(subcommand)]
        command: TopObject,
    },
    Send {
        #[arg(value_enum, default_value_t = SendKind::Command)]
        kind: SendKind,

        /// External command operation helper (restart_mode, stop_mode, activate_mode, deactivate_mode, execute_now)
        #[arg(long)]
        op: Option<String>,

        /// Autonomy mode name for op helpers (e.g. NoImages)
        #[arg(long)]
        mode: Option<String>,

        /// Command name for --op execute_now (e.g. PointNadir)
        #[arg(long)]
        command: Option<String>,

        /// Optional JSON payload. Accepts full ingress JSON or payload-only JSON for selected kind.
        #[arg(long)]
        json: Option<String>,
    },
    Watch {
        #[command(subcommand)]
        command: WatchObject,
    },
    Status {
        #[arg(short, long, default_value_t = false)]
        watch: bool,

        #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
        output: OutputFormat,

        #[arg(long, default_value_t = 1)]
        interval_secs: u64,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum SendKind {
    Command,
    Telemetry,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum MessageKind {
    All,
    Events,
    Effects,
}

#[derive(Subcommand)]
enum WatchObject {
    Messages {
        #[arg(short = 'n', long, default_value_t = 100)]
        tail: usize,

        #[arg(short, long, default_value_t = false)]
        follow: bool,

        #[arg(long, value_enum, default_value_t = MessageKind::All)]
        kind: MessageKind,
    },
    Telemetry {
        #[arg(short = 'n', long, default_value_t = 10)]
        tail: usize,

        #[arg(long)]
        source: Option<String>,

        #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
        output: OutputFormat,
    },
    Board {
        #[arg(long, value_enum)]
        state: Option<BoardStateFilter>,

        #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
        output: OutputFormat,

        #[arg(long, default_value_t = 1)]
        interval_secs: u64,
    },
}

#[derive(Subcommand)]
enum TopObject {
    Modes {
        #[arg(short = 'A', long)]
        all: bool,

        #[arg()]
        name: Option<String>,

        #[arg(short, long, default_value_t = false)]
        watch: bool,

        #[arg(long, default_value_t = false)]
        tui: bool,

        #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
        output: OutputFormat,

        #[arg(long, default_value_t = 1)]
        interval_secs: u64,
    },
}

#[derive(Debug, Clone, serde::Serialize)]
struct ModeView {
    name: String,
    id: Uuid,
    priority: Option<u8>,
    enabled: Option<bool>,
    eligible: Option<bool>,
    active: bool,
    selection_reason: Option<String>,
    connection: Option<ModeConnectionState>,
    handler: Option<ModeHandlerState>,
    detail: Option<String>,
}

fn print_table_snapshot(rendered: &str, first_snapshot: &mut bool) {
    if !*first_snapshot {
        if io::stdout().is_terminal() {
            print!("{}", terminal_clear_sequence());
        } else {
            println!("\n--- update {} ---", Utc::now().to_rfc3339());
        }
    }
    println!("{rendered}");
    *first_snapshot = false;
}

fn terminal_clear_sequence() -> &'static str {
    "\x1b[2J\x1b[H"
}

fn default_runtime_config_path() -> String {
    let cwd_candidate = PathBuf::from("safe/safe.yaml");
    if cwd_candidate.exists() {
        return cwd_candidate.to_string_lossy().to_string();
    }
    "/opt/safe/safe.yaml".to_string()
}

fn resolve_runtime_config_path() -> String {
    var("SAFE_RUNTIME_CONFIG")
        .or_else(|_| var("SAFE_RUNTIME_CONFIG_PATH"))
        .unwrap_or_else(|_| default_runtime_config_path())
}

fn default_mode_config_path() -> String {
    let cwd_candidate = PathBuf::from("safe/autonomy_mode_config.json");
    if cwd_candidate.exists() {
        return cwd_candidate.to_string_lossy().to_string();
    }
    "/opt/safe/autonomy_mode_config.json".to_string()
}

async fn load_runtime_config() -> anyhow::Result<RuntimeConfigView> {
    let path = resolve_runtime_config_path();
    let contents = fs::read_to_string(path).await?;
    let mut cfg: RuntimeConfigView = serde_yaml::from_str(&contents)?;
    if let Ok(file_path) = var("SAFE_LOGGING__FILE_PATH") {
        cfg.logging.file_path = file_path;
    }
    if let Ok(base_directory) = var("SAFE_BASE_PATHS__BASE_WRITABLE_DIRECTORY") {
        cfg.base_paths.base_writable_directory = base_directory;
    }
    Ok(cfg)
}

async fn load_mode_config() -> anyhow::Result<Vec<AutonomyModeConfigItem>> {
    let path = var("SAFE_AUTONOMY_MODE_CONFIG_PATH").unwrap_or_else(|_| default_mode_config_path());
    let contents = fs::read_to_string(path).await?;
    Ok(serde_json::from_str::<Vec<AutonomyModeConfigItem>>(
        &contents,
    )?)
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ModeConfigDocumentItem {
    name: String,
    priority: u8,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    activation: Option<Value>,
    #[serde(default)]
    mode_config: Value,
}

fn default_true() -> bool {
    true
}

async fn load_mode_config_document() -> anyhow::Result<Vec<ModeConfigDocumentItem>> {
    let path = var("SAFE_AUTONOMY_MODE_CONFIG_PATH").unwrap_or_else(|_| default_mode_config_path());
    let contents = fs::read_to_string(path).await?;
    Ok(serde_json::from_str::<Vec<ModeConfigDocumentItem>>(
        &contents,
    )?)
}

async fn load_flight(base_writable_directory: &str) -> anyhow::Result<FlightCheckpoint> {
    let path = Path::new(base_writable_directory)
        .join("state")
        .join("flight.json");
    if !path.exists() {
        return Ok(FlightCheckpoint {
            active_autonomy_mode: None,
            autonomy_modes: vec![],
        });
    }
    let contents = fs::read_to_string(path).await?;
    Ok(serde_json::from_str(&contents)?)
}

async fn load_operational_status(
    base_writable_directory: &str,
) -> anyhow::Result<OperationalStatus> {
    let path = state_dir(base_writable_directory).join("status.json");
    let contents = fs::read_to_string(&path).await.map_err(|e| {
        anyhow::anyhow!(
            "Operational status is unavailable at {}: {e}",
            path.display()
        )
    })?;
    Ok(serde_json::from_str(&contents)?)
}

fn build_mode_views(
    config_modes: &[AutonomyModeConfigItem],
    flight: &FlightCheckpoint,
) -> Vec<ModeView> {
    let mut meta_by_id = HashMap::new();
    for meta in &flight.autonomy_modes {
        meta_by_id.insert(meta.id.0, meta);
    }

    let mut rows = Vec::with_capacity(config_modes.len());
    for mode in config_modes {
        let id = mode_id_from_name(&mode.name);
        let meta = meta_by_id.get(&id.0).copied();
        rows.push(ModeView {
            name: mode.name.clone(),
            id: id.0,
            priority: meta.map(|m| m.priority).or(Some(mode.priority)),
            enabled: meta.map(|m| m.enabled).or(Some(mode.enabled)),
            eligible: None,
            active: flight.active_autonomy_mode == Some(id),
            selection_reason: None,
            connection: None,
            handler: None,
            detail: None,
        });
    }

    rows.sort_by(|a, b| {
        b.priority
            .unwrap_or(0)
            .cmp(&a.priority.unwrap_or(0))
            .then_with(|| a.name.cmp(&b.name))
    });
    rows
}

fn mode_view_from_status(mode: &ModeOperationalStatus) -> ModeView {
    ModeView {
        name: mode.name.clone(),
        id: mode.id.0,
        priority: Some(mode.priority),
        enabled: Some(mode.enabled),
        eligible: Some(mode.eligible),
        active: mode.active,
        selection_reason: Some(mode.selection_reason.clone()),
        connection: Some(mode.runtime.connection.clone()),
        handler: mode.runtime.handler.clone(),
        detail: mode.runtime.detail.clone(),
    }
}

fn print_modes_table(modes: &[ModeView]) {
    println!("{}", render_modes_table(modes));
}

fn print_modes_json(modes: &[ModeView]) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(modes)?);
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
struct ModeDescribeView {
    name: String,
    id: Uuid,
    active: bool,
    enabled: bool,
    priority: u8,
    activation: Option<Value>,
    mode_config: Value,
}

fn print_mode_describe_table(view: &ModeDescribeView) {
    println!("{}", render_mode_describe_table(view));
}

fn print_mode_describe_json(view: &ModeDescribeView) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(view)?);
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
struct ModeTopRow {
    mode: ModeView,
    snapshot: Option<ModeResourceSnapshot>,
}

async fn load_mode_snapshot(
    mode_id: Uuid,
    base_writable_directory: &str,
) -> Option<ModeResourceSnapshot> {
    let base = Path::new(base_writable_directory)
        .join("state")
        .join("modes");
    let candidates = [
        base.join(mode_id.to_string()).join("metrics-current.json"),
        base.join(format!("autonomymodeid({mode_id})"))
            .join("metrics-current.json"),
    ];

    for path in candidates {
        if let Ok(contents) = fs::read_to_string(&path).await
            && let Ok(snapshot) = serde_json::from_str::<ModeResourceSnapshot>(&contents)
        {
            return Some(snapshot);
        }
    }

    None
}

async fn safe_running(base_writable_directory: &str) -> bool {
    let pid_path = Path::new(base_writable_directory)
        .join("state")
        .join("safe.pid");
    let pid_text = match fs::read_to_string(pid_path).await {
        Ok(v) => v,
        Err(_) => return false,
    };
    let pid = match pid_text.trim().parse::<u32>() {
        Ok(v) => v,
        Err(_) => return false,
    };

    let output = TokioCommand::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .await;

    matches!(output, Ok(o) if o.status.success())
}

fn state_dir(base_writable_directory: &str) -> PathBuf {
    Path::new(base_writable_directory).join("state")
}

fn render_top_json(rows: &[ModeTopRow]) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(rows)?);
    Ok(())
}

fn sort_top_rows_by_cpu(rows: &mut [ModeTopRow]) {
    rows.sort_by(|a, b| {
        let ac = a
            .snapshot
            .as_ref()
            .map(|s| s.cpu_percent)
            .unwrap_or(f64::MIN);
        let bc = b
            .snapshot
            .as_ref()
            .map(|s| s.cpu_percent)
            .unwrap_or(f64::MIN);
        bc.total_cmp(&ac)
            .then_with(|| a.mode.name.cmp(&b.mode.name))
    });
}

type TopTerminal = ratatui::Terminal<CrosstermBackend<io::Stdout>>;

struct TopTerminalGuard {
    terminal: TopTerminal,
}

impl TopTerminalGuard {
    fn enter() -> anyhow::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = ratatui::Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    fn terminal_mut(&mut self) -> &mut TopTerminal {
        &mut self.terminal
    }
}

impl Drop for TopTerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), Show, LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

fn draw_top_tui(
    terminal: &mut TopTerminal,
    rows: &[ModeTopRow],
    interval_secs: u64,
    show_children: bool,
    safe_is_running: bool,
) -> anyhow::Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        let chunks = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

        let status = if safe_is_running {
            "RUNNING"
        } else {
            "STOPPED"
        };
        let summary = Paragraph::new(format!(
            "SAFE: {status} | refresh: {}s | children: {}",
            interval_secs.max(1),
            if show_children { "shown" } else { "hidden" }
        ))
        .block(Block::default().borders(Borders::ALL).title("safectl top"));
        f.render_widget(summary, chunks[0]);

        let mut table_rows = Vec::new();
        for row in rows {
            let cells = top_row_values(row)
                .into_iter()
                .map(TuiCell::from)
                .collect::<Vec<_>>();
            table_rows.push(TuiRow::new(cells));

            if show_children
                && let Some(snapshot) = &row.snapshot
                && !snapshot.processes.is_empty()
            {
                table_rows.extend(process_tree_rows(&snapshot.processes).into_iter().map(
                    |cells| TuiRow::new(cells).style(Style::default().add_modifier(Modifier::DIM)),
                ));
            }
        }

        let widths = [
            Constraint::Percentage(27),
            Constraint::Percentage(10),
            Constraint::Percentage(9),
            Constraint::Percentage(13),
            Constraint::Percentage(13),
            Constraint::Percentage(13),
            Constraint::Percentage(8),
            Constraint::Percentage(7),
        ];
        let table = TuiTable::new(table_rows, widths)
            .header(TuiRow::new(top_headers()).style(Style::default().add_modifier(Modifier::BOLD)))
            .column_spacing(1)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("mode resources"),
            );
        f.render_widget(table, chunks[1]);

        let footer = Paragraph::new("c: toggle children | q/Esc: quit | Ctrl-C: quit");
        f.render_widget(footer, chunks[2]);
    })?;
    Ok(())
}

async fn run_top_modes(
    all: bool,
    name: Option<String>,
    watch: bool,
    tui: bool,
    output: OutputFormat,
    interval_secs: u64,
) -> anyhow::Result<()> {
    if output == OutputFormat::Json && (watch || tui) {
        anyhow::bail!("`--output json` is only supported without --watch/--tui");
    }

    let runtime_cfg = load_runtime_config().await?;
    let config_modes = load_mode_config().await?;
    let flight = load_flight(&runtime_cfg.base_paths.base_writable_directory).await?;
    let mut rows = build_mode_views(&config_modes, &flight);

    if let Some(name) = name {
        rows.retain(|m| m.name == name);
    } else if !all {
        rows.retain(|m| m.enabled.unwrap_or(false));
    }

    if rows.is_empty() {
        println!("No modes found");
        return Ok(());
    }

    let effective_watch = watch || tui;

    let mut terminal = if tui {
        Some(TopTerminalGuard::enter()?)
    } else {
        None
    };
    let mut show_children = true;
    let mut quit_requested = false;
    let mut first_snapshot = true;

    loop {
        let safe_is_running = safe_running(&runtime_cfg.base_paths.base_writable_directory).await;
        let mut top_rows = Vec::with_capacity(rows.len());
        for mode in &rows {
            let snapshot = if safe_is_running {
                load_mode_snapshot(mode.id, &runtime_cfg.base_paths.base_writable_directory).await
            } else {
                None
            };
            top_rows.push(ModeTopRow {
                mode: mode.clone(),
                snapshot,
            });
        }

        if effective_watch {
            sort_top_rows_by_cpu(&mut top_rows);
        }

        match output {
            OutputFormat::Table => {
                if tui {
                    if let Some(terminal) = terminal.as_mut() {
                        draw_top_tui(
                            terminal.terminal_mut(),
                            &top_rows,
                            interval_secs,
                            show_children,
                            safe_is_running,
                        )?;
                    }
                } else {
                    let rendered = if safe_is_running {
                        output::render_top_table(&top_rows)
                    } else {
                        format!(
                            "SAFE is not running (stale snapshots hidden).\n\n{}",
                            output::render_top_table(&top_rows)
                        )
                    };
                    if effective_watch {
                        print_table_snapshot(&rendered, &mut first_snapshot);
                    } else {
                        println!("{rendered}");
                    }
                }
            }
            OutputFormat::Json => render_top_json(&top_rows)?,
        }

        if !effective_watch {
            break;
        }

        if tui {
            let tick = Duration::from_millis(100);
            let mut elapsed_ms = 0u64;
            let target_ms = interval_secs.max(1) * 1000;
            loop {
                if elapsed_ms >= target_ms {
                    break;
                }

                if event::poll(std::time::Duration::from_millis(0))? {
                    if let CEvent::Key(key) = event::read()? {
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }
                        match key.code {
                            KeyCode::Char('c') | KeyCode::Char('C') => {
                                if key.modifiers.contains(KeyModifiers::CONTROL) {
                                    quit_requested = true;
                                    break;
                                }
                                show_children = !show_children;
                                break;
                            }
                            KeyCode::Char('q') | KeyCode::Esc => {
                                quit_requested = true;
                                break;
                            }
                            _ => {}
                        }
                    }
                }

                time::sleep(tick).await;
                elapsed_ms += 100;
            }

            if quit_requested {
                break;
            }
        } else {
            tokio::select! {
                _ = time::sleep(Duration::from_secs(interval_secs.max(1))) => {}
                _ = tokio::signal::ctrl_c() => {
                    break;
                }
            }
        }
    }

    Ok(())
}

async fn run_get_modes(
    all: bool,
    name: Option<String>,
    output: OutputFormat,
) -> anyhow::Result<()> {
    let runtime_cfg = load_runtime_config().await?;
    let mut rows =
        match load_operational_status(&runtime_cfg.base_paths.base_writable_directory).await {
            Ok(status) => status.modes.iter().map(mode_view_from_status).collect(),
            Err(_) => {
                let config_modes = load_mode_config().await?;
                let flight = load_flight(&runtime_cfg.base_paths.base_writable_directory).await?;
                build_mode_views(&config_modes, &flight)
            }
        };

    if let Some(name) = name {
        rows.retain(|m| m.name == name);
    } else if !all {
        rows.retain(|m| m.enabled.unwrap_or(false));
    }

    if rows.is_empty() {
        println!("No modes found");
        return Ok(());
    }

    match output {
        OutputFormat::Table => print_modes_table(&rows),
        OutputFormat::Json => print_modes_json(&rows)?,
    }
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
struct TelemetryView {
    seq: Option<u64>,
    source: Option<String>,
    ts_mono: u64,
    payload: Value,
}

fn parse_telemetry_event(line: &str) -> Option<TelemetryView> {
    let event: Value = serde_json::from_str(line).ok()?;
    let frame = event.get("msg")?.get("TelemetryReceived")?.clone();
    let frame: TelemetryFrame = serde_json::from_value(frame).ok()?;
    Some(TelemetryView {
        seq: event.get("seq").and_then(Value::as_u64),
        source: frame.source,
        ts_mono: frame.ts_mono,
        payload: frame.payload,
    })
}

async fn load_telemetry_views(
    events_path: &Path,
    tail: usize,
    source: Option<&str>,
) -> anyhow::Result<Vec<TelemetryView>> {
    if !events_path.exists() {
        return Ok(vec![]);
    }

    let contents = fs::read_to_string(events_path).await?;
    let mut views = VecDeque::new();
    for line in contents.lines() {
        let Some(view) = parse_telemetry_event(line) else {
            continue;
        };
        if source.is_some_and(|source| view.source.as_deref() != Some(source)) {
            continue;
        }
        views.push_back(view);
        if views.len() > tail {
            views.pop_front();
        }
    }
    Ok(views.into_iter().collect())
}

fn print_telemetry_table(views: &[TelemetryView]) {
    println!("{}", render_telemetry_table(views));
}

fn print_telemetry(views: &[TelemetryView], output: OutputFormat) -> anyhow::Result<()> {
    if views.is_empty() {
        println!("No telemetry received");
        return Ok(());
    }
    match output {
        OutputFormat::Table => print_telemetry_table(views),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(views)?),
    }
    Ok(())
}

async fn run_get_telemetry(
    tail: usize,
    source: Option<String>,
    output: OutputFormat,
) -> anyhow::Result<()> {
    let runtime_cfg = load_runtime_config().await?;
    let state = state_dir(&runtime_cfg.base_paths.base_writable_directory);
    let mut views =
        load_telemetry_views(&state.join("events.jsonl"), tail, source.as_deref()).await?;

    if views.is_empty()
        && let Ok(status) =
            load_operational_status(&runtime_cfg.base_paths.base_writable_directory).await
        && let Some(latest) = status.telemetry.latest
        && source
            .as_deref()
            .is_none_or(|source| latest.source.as_deref() == Some(source))
    {
        views.push(TelemetryView {
            seq: None,
            source: latest.source,
            ts_mono: latest.ts_mono,
            payload: latest.payload,
        });
    }

    print_telemetry(&views, output)
}

async fn follow_telemetry(
    events_path: &Path,
    source: Option<&str>,
    output: OutputFormat,
    tail: usize,
    initial_views: Vec<TelemetryView>,
) -> anyhow::Result<()> {
    let mut views = VecDeque::from(initial_views);
    let mut first_snapshot = output != OutputFormat::Table;
    let mut offset = fs::metadata(events_path)
        .await
        .map(|meta| meta.len() as usize)
        .unwrap_or(0);
    let mut tick = time::interval(Duration::from_millis(250));

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let contents = match fs::read_to_string(events_path).await {
                    Ok(contents) => contents,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                };
                if contents.len() < offset {
                    offset = 0;
                    views.clear();
                }
                let new_contents = &contents[offset..];
                offset = contents.len();
                for line in new_contents.lines() {
                    let Some(view) = parse_telemetry_event(line) else {
                        continue;
                    };
                    if source.is_some_and(|source| view.source.as_deref() != Some(source)) {
                        continue;
                    }
                    if output == OutputFormat::Table {
                        views.push_back(view);
                        while views.len() > tail {
                            views.pop_front();
                        }
                        let rendered = if views.is_empty() {
                            "No telemetry received".to_string()
                        } else {
                            render_telemetry_table(views.make_contiguous())
                        };
                        print_table_snapshot(&rendered, &mut first_snapshot);
                    } else {
                        print_telemetry(std::slice::from_ref(&view), output)?;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    Ok(())
}

async fn run_watch_telemetry(
    tail: usize,
    source: Option<String>,
    output: OutputFormat,
) -> anyhow::Result<()> {
    let runtime_cfg = load_runtime_config().await?;
    let events_path =
        state_dir(&runtime_cfg.base_paths.base_writable_directory).join("events.jsonl");
    let views = load_telemetry_views(&events_path, tail, source.as_deref()).await?;
    if output == OutputFormat::Table {
        let rendered = if views.is_empty() {
            "No telemetry received".to_string()
        } else {
            render_telemetry_table(&views)
        };
        let mut first_snapshot = true;
        print_table_snapshot(&rendered, &mut first_snapshot);
    } else {
        print_telemetry(&views, output)?;
    }
    follow_telemetry(&events_path, source.as_deref(), output, tail, views).await
}

fn board_state_matches(state: &BoardCommandState, filter: Option<BoardStateFilter>) -> bool {
    match filter {
        None => true,
        Some(BoardStateFilter::Pending) => matches!(state, BoardCommandState::Pending),
        Some(BoardStateFilter::Approved) => matches!(state, BoardCommandState::Approved),
        Some(BoardStateFilter::Rejected) => matches!(state, BoardCommandState::Rejected),
        Some(BoardStateFilter::Published) => matches!(state, BoardCommandState::Published),
    }
}

fn selected_board_entries(
    entries: &[BoardCommandStatus],
    filter: Option<BoardStateFilter>,
) -> Vec<BoardCommandStatus> {
    entries
        .iter()
        .filter(|entry| board_state_matches(&entry.state, filter))
        .cloned()
        .collect()
}

fn print_board(entries: &[BoardCommandStatus], output: OutputFormat) -> anyhow::Result<()> {
    if entries.is_empty() {
        println!("No board commands found");
        return Ok(());
    }
    match output {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(entries)?),
        OutputFormat::Table => println!("{}", render_board_table(entries)),
    }
    Ok(())
}

async fn run_get_board(
    filter: Option<BoardStateFilter>,
    output: OutputFormat,
) -> anyhow::Result<()> {
    let runtime_cfg = load_runtime_config().await?;
    let status = load_operational_status(&runtime_cfg.base_paths.base_writable_directory).await?;
    let entries = selected_board_entries(&status.board, filter);
    print_board(&entries, output)
}

async fn run_watch_board(
    filter: Option<BoardStateFilter>,
    output: OutputFormat,
    interval_secs: u64,
) -> anyhow::Result<()> {
    let runtime_cfg = load_runtime_config().await?;
    let base = runtime_cfg.base_paths.base_writable_directory;
    let mut previous = None;
    let mut first_snapshot = true;

    loop {
        let status = load_operational_status(&base).await?;
        let entries = selected_board_entries(&status.board, filter);
        if output == OutputFormat::Table {
            let table = if entries.is_empty() {
                "No board commands found".to_string()
            } else {
                render_board_table(&entries)
            };
            print_table_snapshot(&table, &mut first_snapshot);
        } else {
            let rendered = serde_json::to_string(&entries)?;
            if previous.as_ref() != Some(&rendered) {
                print_board(&entries, output)?;
                previous = Some(rendered);
            }
        }
        tokio::select! {
            _ = time::sleep(Duration::from_secs(interval_secs.max(1))) => {}
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    Ok(())
}

fn print_request(statuses: &[HostCommandStatus], output: OutputFormat) -> anyhow::Result<()> {
    if statuses.is_empty() {
        println!("No status found for request");
        return Ok(());
    }
    match output {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(statuses)?),
        OutputFormat::Table => println!("{}", render_request_table(statuses)),
    }
    Ok(())
}

async fn run_get_request(request_id: String, output: OutputFormat) -> anyhow::Result<()> {
    let runtime_cfg = load_runtime_config().await?;
    let path = state_dir(&runtime_cfg.base_paths.base_writable_directory)
        .join("host_command_status.jsonl");
    let contents = match fs::read_to_string(path).await {
        Ok(contents) => contents,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let statuses: Vec<HostCommandStatus> = contents
        .lines()
        .filter_map(|line| serde_json::from_str::<HostCommandStatus>(line).ok())
        .filter(|status| status.request_id == request_id)
        .collect();
    print_request(&statuses, output)
}

fn status_table_text(status: &OperationalStatus, process_alive: bool) -> String {
    let modes: Vec<ModeView> = status.modes.iter().map(mode_view_from_status).collect();
    render_status_table(status, process_alive, &modes)
}

async fn run_status(watch: bool, output: OutputFormat, interval_secs: u64) -> anyhow::Result<()> {
    if watch && output == OutputFormat::Json {
        anyhow::bail!("`--output json` is only supported without --watch");
    }
    let runtime_cfg = load_runtime_config().await?;
    let base = runtime_cfg.base_paths.base_writable_directory;
    let mut first_snapshot = true;

    loop {
        let status = load_operational_status(&base).await?;
        match output {
            OutputFormat::Table => {
                let rendered = status_table_text(&status, safe_running(&base).await);
                if watch {
                    print_table_snapshot(&rendered, &mut first_snapshot);
                } else {
                    println!("{rendered}");
                }
            }
            OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&status)?),
        }
        if !watch {
            break;
        }
        tokio::select! {
            _ = time::sleep(Duration::from_secs(interval_secs.max(1))) => {}
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    Ok(())
}

async fn run_describe_mode(name: String, output: OutputFormat) -> anyhow::Result<()> {
    let runtime_cfg = load_runtime_config().await?;
    let flight = load_flight(&runtime_cfg.base_paths.base_writable_directory).await?;
    let config_doc = load_mode_config_document().await?;

    let item = config_doc
        .into_iter()
        .find(|m| m.name == name)
        .ok_or_else(|| anyhow::anyhow!("Mode not found in config: {name}"))?;

    let id = mode_id_from_name(&item.name);
    let view = ModeDescribeView {
        name: item.name,
        id: id.0,
        active: flight.active_autonomy_mode == Some(id),
        enabled: item.enabled,
        priority: item.priority,
        activation: item.activation,
        mode_config: item.mode_config,
    };

    match output {
        OutputFormat::Table => print_mode_describe_table(&view),
        OutputFormat::Json => print_mode_describe_json(&view)?,
    }

    Ok(())
}

fn default_ingress_for(kind: SendKind) -> SafectlIngress {
    match kind {
        SendKind::Command => SafectlIngress::Command {
            command: ExternalCommand::ExecuteNow {
                command: Command::PointNadir,
            },
            request_id: None,
        },
        SendKind::Telemetry => SafectlIngress::Telemetry {
            telemetry: TelemetryFrame::new(serde_json::json!({})),
        },
    }
}

#[derive(serde::Deserialize)]
struct PlainTelemetryFrame {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    ts_mono: u64,
    payload: Value,
}

fn parse_send_payload(kind: SendKind, json: Option<String>) -> anyhow::Result<SafectlIngress> {
    let Some(json) = json else {
        return Ok(default_ingress_for(kind));
    };

    if let Ok(full) = serde_json::from_str::<SafectlIngress>(&json) {
        return Ok(full);
    }

    match kind {
        SendKind::Command => {
            if let Ok(ec) = serde_json::from_str::<ExternalCommand>(&json) {
                return Ok(SafectlIngress::Command {
                    command: ec,
                    request_id: None,
                });
            }
            if let Ok(cmd) = serde_json::from_str::<Command>(&json) {
                return Ok(SafectlIngress::Command {
                    command: ExternalCommand::ExecuteNow { command: cmd },
                    request_id: None,
                });
            }
            anyhow::bail!(
                "Invalid command payload. Provide full ingress JSON or an ExternalCommand/Command JSON."
            )
        }
        SendKind::Telemetry => {
            if let Ok(frame) = serde_json::from_str::<TelemetryFrame>(&json) {
                return Ok(SafectlIngress::Telemetry { telemetry: frame });
            }
            if let Ok(frame) = serde_json::from_str::<PlainTelemetryFrame>(&json) {
                return Ok(SafectlIngress::Telemetry {
                    telemetry: TelemetryFrame {
                        source: frame.source,
                        ts_mono: frame.ts_mono,
                        payload: frame.payload,
                    },
                });
            }
            if let Ok(payload) = serde_json::from_str::<Value>(&json) {
                return Ok(SafectlIngress::Telemetry {
                    telemetry: TelemetryFrame::new(payload),
                });
            }
            anyhow::bail!(
                "Invalid telemetry payload. Provide full ingress JSON, a telemetry frame, or a JSON payload object."
            )
        }
    }
}

fn read_from_editor(initial_json: &str) -> anyhow::Result<String> {
    let editor = var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let mut path = std::env::temp_dir();
    path.push(format!("safectl-send-{}.json", std::process::id()));
    std::fs::write(&path, initial_json)?;
    let status = std::process::Command::new(editor).arg(&path).status()?;
    if !status.success() {
        anyhow::bail!("Editor exited unsuccessfully");
    }
    let edited = std::fs::read_to_string(&path)?;
    let _ = std::fs::remove_file(&path);
    Ok(edited)
}

fn parse_command_name(name: &str) -> anyhow::Result<Command> {
    match name {
        "SetPidControllerGains" => Ok(Command::SetPidControllerGains(0.0, 0.0, 0.0, 0.0)),
        "IridiumPowerOn" => Ok(Command::IridiumPowerOn),
        "IridiumPowerOff" => Ok(Command::IridiumPowerOff),
        "PointSunYaw" => Ok(Command::PointSunYaw),
        "PointNadir" => Ok(Command::PointNadir),
        "CaptureImage" => Ok(Command::CaptureImage),
        "PointThruster" => Ok(Command::PointThruster),
        "ThrusterOn" => Ok(Command::ThrusterOn),
        "ThrusterOff" => Ok(Command::ThrusterOff),
        _ => anyhow::bail!(
            "Unknown command `{name}`. Try PointNadir, PointSunYaw, CaptureImage, PointThruster, ThrusterOn, ThrusterOff, IridiumPowerOn, IridiumPowerOff"
        ),
    }
}

fn build_ingress_from_helper(
    op: Option<String>,
    mode: Option<String>,
    command: Option<String>,
) -> anyhow::Result<Option<SafectlIngress>> {
    let Some(op) = op else {
        return Ok(None);
    };

    let ingress = match op.as_str() {
        "restart_mode" => {
            let name =
                mode.ok_or_else(|| anyhow::anyhow!("--mode is required for --op restart_mode"))?;
            let id = mode_id_from_name(&name);
            SafectlIngress::Command {
                command: ExternalCommand::RestartMode { mode: id },
                request_id: None,
            }
        }
        "stop_mode" => {
            let name =
                mode.ok_or_else(|| anyhow::anyhow!("--mode is required for --op stop_mode"))?;
            let id = mode_id_from_name(&name);
            SafectlIngress::Command {
                command: ExternalCommand::StopMode { mode: id },
                request_id: None,
            }
        }
        "activate_mode" => {
            let name =
                mode.ok_or_else(|| anyhow::anyhow!("--mode is required for --op activate_mode"))?;
            let id = mode_id_from_name(&name);
            SafectlIngress::Command {
                command: ExternalCommand::ActivateMode { mode: id },
                request_id: None,
            }
        }
        "deactivate_mode" => {
            let name =
                mode.ok_or_else(|| anyhow::anyhow!("--mode is required for --op deactivate_mode"))?;
            let id = mode_id_from_name(&name);
            SafectlIngress::Command {
                command: ExternalCommand::DeactivateMode { mode: id },
                request_id: None,
            }
        }
        "execute_now" => {
            let cmd_name = command
                .ok_or_else(|| anyhow::anyhow!("--command is required for --op execute_now"))?;
            let cmd = parse_command_name(&cmd_name)?;
            SafectlIngress::Command {
                command: ExternalCommand::ExecuteNow { command: cmd },
                request_id: None,
            }
        }
        _ => anyhow::bail!(
            "Unknown --op `{op}`. Use restart_mode, stop_mode, activate_mode, deactivate_mode, execute_now"
        ),
    };

    Ok(Some(ingress))
}

async fn run_send_with_helper(
    kind: SendKind,
    op: Option<String>,
    mode: Option<String>,
    command: Option<String>,
    json: Option<String>,
) -> anyhow::Result<()> {
    let helper = build_ingress_from_helper(op, mode, command)?;
    let runtime_cfg = load_runtime_config().await?;
    let state = state_dir(&runtime_cfg.base_paths.base_writable_directory);
    let sock_path = state.join("safectl.sock");
    if !sock_path.exists() {
        anyhow::bail!("SAFE ingress socket not found at {}", sock_path.display());
    }

    let mut message = if let Some(m) = helper {
        if json.is_some() {
            anyhow::bail!("Use either helper flags (--op/--mode/--command) or --json, not both");
        }
        m
    } else if json.is_none() {
        let template = serde_json::to_string_pretty(&default_ingress_for(kind))?;
        let edited = read_from_editor(&template)?;
        parse_send_payload(kind, Some(edited))?
    } else {
        parse_send_payload(kind, json)?
    };

    let request_id = match &mut message {
        SafectlIngress::Command { request_id, .. } => {
            let id = request_id
                .clone()
                .unwrap_or_else(|| format!("safectl:{}", Uuid::new_v4()));
            *request_id = Some(id.clone());
            Some(id)
        }
        SafectlIngress::Telemetry { .. } => None,
    };

    let mut stream = UnixStream::connect(&sock_path).await?;
    let wire = serde_json::to_string(&message)?;
    stream.write_all(wire.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;
    if let Some(request_id) = request_id {
        println!("sent request_id={request_id}");
    } else {
        println!("sent");
    }
    Ok(())
}

fn format_jsonl_line(kind: &str, line: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) => {
            let head = match kind {
                "event" => format!(
                    "event {}",
                    v.get("seq").and_then(|x| x.as_u64()).unwrap_or(0)
                ),
                _ => "effect".to_string(),
            };
            format!("[{head}] {v}")
        }
        Err(_) => format!("[{kind}] {line}"),
    }
}

async fn read_jsonl_tail(path: &Path, tail: usize) -> anyhow::Result<Vec<String>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let mut file = fs::File::open(path).await?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await?;
    let content = String::from_utf8_lossy(&buf);
    let mut out: VecDeque<String> = VecDeque::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push_back(line.to_string());
        if out.len() > tail {
            out.pop_front();
        }
    }
    Ok(out.into_iter().collect())
}

async fn follow_jsonl(path: &Path, tag: &'static str) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let file = fs::File::open(path).await?;
    let mut reader = BufReader::new(file);
    let mut offset = reader.seek(std::io::SeekFrom::End(0)).await?;
    let mut tick = time::interval(Duration::from_millis(250));

    loop {
        tick.tick().await;
        let metadata = fs::metadata(path).await?;
        let len = metadata.len();
        if len < offset {
            offset = 0;
            reader.seek(std::io::SeekFrom::Start(0)).await?;
        }
        if len == offset {
            continue;
        }

        reader.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut line = String::new();
        loop {
            line.clear();
            let read = reader.read_line(&mut line).await?;
            if read == 0 {
                break;
            }
            let clean = line.trim_end_matches(['\n', '\r']);
            if clean.is_empty() {
                continue;
            }
            println!("{}", format_jsonl_line(tag, clean));
        }
        offset = reader.seek(std::io::SeekFrom::Current(0)).await?;
    }
}

async fn run_watch_messages(tail: usize, follow: bool, kind: MessageKind) -> anyhow::Result<()> {
    let runtime_cfg = load_runtime_config().await?;
    let state = state_dir(&runtime_cfg.base_paths.base_writable_directory);
    let events_path = state.join("events.jsonl");
    let outputs_path = state.join("outputs.jsonl");

    if matches!(kind, MessageKind::All | MessageKind::Events) {
        let lines = read_jsonl_tail(&events_path, tail).await?;
        for line in lines {
            println!("{}", format_jsonl_line("event", &line));
        }
    }

    if matches!(kind, MessageKind::All | MessageKind::Effects) {
        let lines = read_jsonl_tail(&outputs_path, tail).await?;
        for line in lines {
            println!("{}", format_jsonl_line("effect", &line));
        }
    }

    if !follow {
        return Ok(());
    }

    match kind {
        MessageKind::Events => follow_jsonl(&events_path, "event").await?,
        MessageKind::Effects => follow_jsonl(&outputs_path, "effect").await?,
        MessageKind::All => {
            tokio::select! {
                res = follow_jsonl(&events_path, "event") => { res?; }
                res = follow_jsonl(&outputs_path, "effect") => { res?; }
            }
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Get { command }) => match command {
            GetObject::Modes { all, name, output } => run_get_modes(all, name, output).await?,
            GetObject::Telemetry {
                tail,
                source,
                output,
            } => run_get_telemetry(tail, source, output).await?,
            GetObject::Board { state, output } => run_get_board(state, output).await?,
            GetObject::Request { request_id, output } => {
                run_get_request(request_id, output).await?
            }
        },
        Some(Commands::Describe { command }) => match command {
            DescribeObject::Mode { name, output } => run_describe_mode(name, output).await?,
        },
        Some(Commands::Logs {
            mode,
            mode_name,
            id,
            all_modes,
            tail,
            follow,
            since,
            before,
            filter,
            level,
            output,
        }) => {
            run_logs(
                mode, mode_name, id, all_modes, tail, follow, since, before, filter, level, output,
            )
            .await?;
        }
        Some(Commands::Top { command }) => match command {
            TopObject::Modes {
                all,
                name,
                watch,
                tui,
                output,
                interval_secs,
            } => run_top_modes(all, name, watch, tui, output, interval_secs).await?,
        },
        Some(Commands::Send {
            kind,
            op,
            mode,
            command,
            json,
        }) => {
            run_send_with_helper(kind, op, mode, command, json).await?;
        }
        Some(Commands::Watch { command }) => match command {
            WatchObject::Messages { tail, follow, kind } => {
                run_watch_messages(tail, follow, kind).await?
            }
            WatchObject::Telemetry {
                tail,
                source,
                output,
            } => run_watch_telemetry(tail, source, output).await?,
            WatchObject::Board {
                state,
                output,
                interval_secs,
            } => run_watch_board(state, output, interval_secs).await?,
        },
        Some(Commands::Status {
            watch,
            output,
            interval_secs,
        }) => run_status(watch, output, interval_secs).await?,
        None => {
            Cli::command().print_help()?;
            println!();
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_redraw_clears_the_screen() {
        assert_eq!(terminal_clear_sequence(), "\x1b[2J\x1b[H");
    }

    #[test]
    fn sort_top_rows_by_cpu_descending() {
        let mut rows = vec![
            ModeTopRow {
                mode: ModeView {
                    name: "A".to_string(),
                    id: Uuid::from_u128(1),
                    priority: Some(1),
                    enabled: Some(true),
                    eligible: None,
                    active: false,
                    selection_reason: None,
                    connection: None,
                    handler: None,
                    detail: None,
                },
                snapshot: Some(ModeResourceSnapshot {
                    mode_id: mode_id_from_name("A"),
                    timestamp_unix_ms: 1,
                    cpu_percent: 1.0,
                    memory_bytes: 1,
                    disk_read_bytes: 1,
                    disk_written_bytes: 1,
                    process_count: 1,
                    processes: vec![],
                    avg_cpu_30_min: 0.0,
                    avg_memory_30_min: 0,
                    max_cpu_30_min: 0.0,
                    max_memory_30_min: 0,
                    min_cpu_30_min: 0.0,
                    min_memory_30_min: 0,
                }),
            },
            ModeTopRow {
                mode: ModeView {
                    name: "B".to_string(),
                    id: Uuid::from_u128(2),
                    priority: Some(1),
                    enabled: Some(true),
                    eligible: None,
                    active: false,
                    selection_reason: None,
                    connection: None,
                    handler: None,
                    detail: None,
                },
                snapshot: Some(ModeResourceSnapshot {
                    mode_id: mode_id_from_name("B"),
                    timestamp_unix_ms: 1,
                    cpu_percent: 9.0,
                    memory_bytes: 1,
                    disk_read_bytes: 1,
                    disk_written_bytes: 1,
                    process_count: 1,
                    processes: vec![],
                    avg_cpu_30_min: 0.0,
                    avg_memory_30_min: 0,
                    max_cpu_30_min: 0.0,
                    max_memory_30_min: 0,
                    min_cpu_30_min: 0.0,
                    min_memory_30_min: 0,
                }),
            },
        ];

        sort_top_rows_by_cpu(&mut rows);
        assert_eq!(rows[0].mode.name, "B");
        assert_eq!(rows[1].mode.name, "A");
    }

    #[test]
    fn resolve_runtime_config_path_prefers_safe_runtime_config() {
        unsafe {
            std::env::set_var("SAFE_RUNTIME_CONFIG", "/tmp/a.yaml");
            std::env::set_var("SAFE_RUNTIME_CONFIG_PATH", "/tmp/b.yaml");
        }
        assert_eq!(resolve_runtime_config_path(), "/tmp/a.yaml");
    }

    #[test]
    fn parse_telemetry_event_decodes_payload_json() {
        let line = serde_json::json!({
            "seq": 9,
            "msg": {
                "TelemetryReceived": {
                    "source": "sim",
                    "ts_mono": 42,
                    "payload": "{\"thermal\":{\"value_c\":34.5}}"
                }
            }
        })
        .to_string();

        let view = parse_telemetry_event(&line).expect("telemetry event");
        assert_eq!(view.seq, Some(9));
        assert_eq!(view.source.as_deref(), Some("sim"));
        assert_eq!(view.payload["thermal"]["value_c"], 34.5);
    }

    #[test]
    fn parse_send_payload_preserves_plain_telemetry_frame_source() {
        let input = serde_json::json!({
            "source": "sensor",
            "ts_mono": 42_u64,
            "payload": {"environment": {"temperature_c": 20.0}}
        })
        .to_string();
        let ingress = parse_send_payload(SendKind::Telemetry, Some(input))
            .expect("plain telemetry frame should parse");
        let wire = serde_json::to_string(&ingress).expect("serialize telemetry ingress");
        let SafectlIngress::Telemetry { telemetry } =
            serde_json::from_str(&wire).expect("deserialize telemetry ingress")
        else {
            panic!("expected telemetry ingress");
        };
        assert_eq!(telemetry.source.as_deref(), Some("sensor"));
        assert_eq!(telemetry.ts_mono, 42);
        assert_eq!(telemetry.payload["environment"]["temperature_c"], 20.0);
    }

    #[test]
    fn payload_preview_truncates_long_values() {
        let preview = output::payload_preview(&serde_json::json!({"value": "x".repeat(100)}));
        assert!(preview.ends_with("..."));
    }
}
