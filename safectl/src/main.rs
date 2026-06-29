use std::collections::{HashMap, HashSet, VecDeque};
use std::env::var;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use clap::{ArgAction, CommandFactory, Parser, Subcommand, ValueEnum};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event as CEvent, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use safe::protocol::Command;
use safe::runtime::{
    AutonomyModeConfigItem, ExternalCommand, FlightCheckpoint, ModeResourceSnapshot,
    ProcessResourceSnapshot, RuntimeConfigView, SafectlIngress, mode_id_from_name,
};
use safe::telemetry_frame::TelemetryFrame;
use serde_json::Value;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command as TokioCommand;
use tokio::time::{self, Duration};
use uuid::Uuid;

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_TRACE: &str = "\x1b[90m";
const ANSI_DEBUG: &str = "\x1b[34m";
const ANSI_INFO: &str = "\x1b[32m";
const ANSI_WARN: &str = "\x1b[33m";
const ANSI_ERROR: &str = "\x1b[31m";

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
    active: bool,
}

#[derive(Debug, Clone)]
struct LogFilter {
    since: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
    filter: Option<String>,
    level: Option<String>,
}

impl LogFilter {
    fn new(
        since: Option<String>,
        before: Option<String>,
        filter: Option<String>,
        level: Option<String>,
    ) -> anyhow::Result<Self> {
        let since = since.map(|s| parse_timestamp(&s)).transpose()?;
        let before = before.map(|s| parse_timestamp(&s)).transpose()?;
        Ok(Self {
            since,
            before,
            filter,
            level: level.map(|l| l.to_uppercase()),
        })
    }

    fn matches(&self, line: &str) -> bool {
        if let Some(level) = &self.level {
            let needle = format!(" {level} ");
            if !line.contains(&needle) {
                return false;
            }
        }

        if let Some(substr) = &self.filter
            && !line.contains(substr)
        {
            return false;
        }

        if self.since.is_none() && self.before.is_none() {
            return true;
        }

        let ts = match parse_line_timestamp(line) {
            Some(ts) => ts,
            None => return true,
        };

        if let Some(since) = self.since
            && ts < since
        {
            return false;
        }

        if let Some(before) = self.before
            && ts > before
        {
            return false;
        }

        true
    }
}

fn parse_timestamp(s: &str) -> anyhow::Result<DateTime<Utc>> {
    let ts = DateTime::parse_from_rfc3339(s)
        .map_err(|e| anyhow::anyhow!("Invalid timestamp `{s}` (expected RFC3339): {e}"))?;
    Ok(ts.with_timezone(&Utc))
}

fn parse_line_timestamp(line: &str) -> Option<DateTime<Utc>> {
    let ts = line.split_whitespace().next()?;
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn colorize_log_line(line: &str) -> String {
    let color = if line.contains(" TRACE ") {
        Some(ANSI_TRACE)
    } else if line.contains(" DEBUG ") {
        Some(ANSI_DEBUG)
    } else if line.contains(" INFO ") {
        Some(ANSI_INFO)
    } else if line.contains(" WARN ") {
        Some(ANSI_WARN)
    } else if line.contains(" ERROR ") {
        Some(ANSI_ERROR)
    } else {
        None
    };

    match color {
        Some(color) => format!("{color}{line}{ANSI_RESET}"),
        None => line.to_string(),
    }
}

fn print_log_line(prefix: Option<&str>, line: &str) {
    let rendered = colorize_log_line(line);
    if let Some(p) = prefix {
        println!("[{p}] {rendered}");
    } else {
        println!("{rendered}");
    }
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
    let cfg: RuntimeConfigView = serde_yaml::from_str(&contents)?;
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
            active: flight.active_autonomy_mode == Some(id),
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

fn print_modes_table(modes: &[ModeView]) {
    println!(
        "{:<36} {:<36} {:>8} {:>8} {:>6}",
        "NAME", "ID", "PRIORITY", "ENABLED", "ACTIVE"
    );
    for mode in modes {
        let prio = mode
            .priority
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string());
        let enabled = mode
            .enabled
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<36} {:<36} {:>8} {:>8} {:>6}",
            mode.name, mode.id, prio, enabled, mode.active
        );
    }
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

fn render_json_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn print_mode_describe_table(view: &ModeDescribeView) {
    println!("Name: {}", view.name);
    println!("ID: {}", view.id);
    println!("Active: {}", view.active);
    println!("Enabled: {}", view.enabled);
    println!("Priority: {}", view.priority);
    println!(
        "Activation: {}",
        view.activation
            .as_ref()
            .map(render_json_pretty)
            .unwrap_or_else(|| "(none)".to_string())
    );
    println!("Mode Config: {}", render_json_pretty(&view.mode_config));
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

fn format_mb(bytes: u64) -> String {
    format!("{:.1}", (bytes as f64) / (1024.0 * 1024.0))
}

fn format_age_secs(ts_unix_ms: u64) -> String {
    let now_ms = Utc::now().timestamp_millis().max(0) as u64;
    if now_ms <= ts_unix_ms {
        "0s".to_string()
    } else {
        format!("{}s", (now_ms - ts_unix_ms) / 1000)
    }
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

fn render_top_table(rows: &[ModeTopRow]) {
    println!("NAME\tACTIVE\tCPU%\tMEM_MB\tDISK_READ\tDISK_WRITE\tPROCS\tAGE");
    for row in rows {
        if let Some(s) = &row.snapshot {
            println!(
                "{}\t{}\t{:.1}\t{}\t{}\t{}\t{}\t{}",
                row.mode.name,
                row.mode.active,
                s.cpu_percent,
                format_mb(s.memory_bytes),
                s.disk_read_bytes,
                s.disk_written_bytes,
                s.process_count,
                format_age_secs(s.timestamp_unix_ms)
            );
        } else {
            println!("{}\t{}\t-\t-\t-\t-\t-\t-", row.mode.name, row.mode.active);
        }
    }
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

fn process_command_label(command: &str) -> String {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        "<unknown>".to_string()
    } else {
        trimmed
            .split_whitespace()
            .next()
            .unwrap_or(trimmed)
            .to_string()
    }
}

fn process_children(processes: &[ProcessResourceSnapshot], parent_pid: u32) -> Vec<usize> {
    let mut children: Vec<usize> = processes
        .iter()
        .enumerate()
        .filter(|(_, p)| p.parent_pid == Some(parent_pid))
        .map(|(idx, _)| idx)
        .collect();
    children.sort_by_key(|idx| processes[*idx].pid);
    children
}

fn collect_process_tree_node_lines(
    processes: &[ProcessResourceSnapshot],
    idx: usize,
    depth: usize,
    lines: &mut Vec<String>,
) {
    let proc = &processes[idx];
    let indent = "  ".repeat(depth);
    lines.push(format!(
        "{}└─ {:<18} {:<8} {:>6.1} {:>8} {:>12} {:>12} {:>6} {:>5}",
        indent,
        format!("{} ({})", process_command_label(&proc.command), proc.pid),
        "child",
        proc.cpu_percent,
        format_mb(proc.memory_bytes),
        proc.disk_read_bytes,
        proc.disk_written_bytes,
        "-",
        "-"
    ));

    for child_idx in process_children(processes, proc.pid) {
        collect_process_tree_node_lines(processes, child_idx, depth + 1, lines);
    }
}

fn collect_process_tree_lines(
    processes: &[ProcessResourceSnapshot],
    depth: usize,
    lines: &mut Vec<String>,
) {
    let pid_set: HashSet<u32> = processes.iter().map(|p| p.pid).collect();
    let mut roots: Vec<usize> = processes
        .iter()
        .enumerate()
        .filter(|(_, p)| match p.parent_pid {
            None => true,
            Some(parent) => !pid_set.contains(&parent),
        })
        .map(|(idx, _)| idx)
        .collect();

    if roots.is_empty() {
        roots = (0..processes.len()).collect();
    }
    roots.sort_by_key(|idx| processes[*idx].pid);

    for idx in roots {
        let proc = &processes[idx];
        let indent = "  ".repeat(depth);
        lines.push(format!(
            "{}└─ {:<18} {:<8} {:>6.1} {:>8} {:>12} {:>12} {:>6} {:>5}",
            indent,
            format!("{} ({})", process_command_label(&proc.command), proc.pid),
            "child",
            proc.cpu_percent,
            format_mb(proc.memory_bytes),
            proc.disk_read_bytes,
            proc.disk_written_bytes,
            "-",
            "-"
        ));
        for child_idx in process_children(processes, proc.pid) {
            collect_process_tree_node_lines(processes, child_idx, depth + 1, lines);
        }
    }
}

fn render_top_tui_lines(
    rows: &[ModeTopRow],
    interval_secs: u64,
    show_children: bool,
    safe_is_running: bool,
) -> Vec<String> {
    let mut lines = vec![
        format!("SAFE Top Modes (refresh {}s)", interval_secs.max(1)),
        format!(
            "Children: {}  |  Keys: c=toggle children, q/esc=quit, ctrl-c=quit",
            if show_children { "ON" } else { "OFF" }
        ),
        String::new(),
    ];

    if !safe_is_running {
        lines.push("SAFE is not running (stale snapshots hidden).".to_string());
        lines.push(String::new());
    }

    lines.push(
        "NAME                                 ACTIVE     CPU%    MEM_MB   DISK_READ     DISK_WRITE    PROCS  AGE"
            .to_string(),
    );
    lines.push(
        "-------------------------------------------------------------------------------------------------------------------"
            .to_string(),
    );

    for row in rows {
        if let Some(s) = &row.snapshot {
            lines.push(format!(
                "{:<36} {:<8} {:>6.1} {:>8} {:>12} {:>14} {:>8} {:>4}",
                row.mode.name,
                row.mode.active,
                s.cpu_percent,
                format_mb(s.memory_bytes),
                s.disk_read_bytes,
                s.disk_written_bytes,
                s.process_count,
                format_age_secs(s.timestamp_unix_ms)
            ));

            if show_children && !s.processes.is_empty() {
                collect_process_tree_lines(&s.processes, 1, &mut lines);
            }
        } else {
            lines.push(format!(
                "{:<36} {:<8} {:>6} {:>8} {:>12} {:>14} {:>8} {:>4}",
                row.mode.name, row.mode.active, "-", "-", "-", "-", "-", "-"
            ));
        }
    }

    lines
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
    let lines = render_top_tui_lines(rows, interval_secs, show_children, safe_is_running);
    let text = lines.join("\n");
    terminal.draw(|f| {
        let size = f.area();
        let widget = Paragraph::new(text.as_str())
            .block(Block::default().borders(Borders::ALL).title("safectl top"))
            .wrap(Wrap { trim: false });
        f.render_widget(widget, size);
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
                    if !safe_is_running {
                        println!("SAFE is not running (stale snapshots hidden).\n");
                    }
                    render_top_table(&top_rows);
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

async fn read_last_lines(
    path: &Path,
    tail: usize,
    filter: &LogFilter,
) -> anyhow::Result<Vec<String>> {
    let mut file = fs::File::open(path).await?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await?;
    let content = String::from_utf8_lossy(&buf);
    let mut out: VecDeque<String> = VecDeque::new();
    for line in content.lines() {
        if filter.matches(line) {
            out.push_back(line.to_string());
            if out.len() > tail {
                out.pop_front();
            }
        }
    }
    Ok(out.into_iter().collect())
}

async fn print_and_follow_file(
    path: &Path,
    tail: usize,
    follow: bool,
    prefix: Option<&str>,
    filter: &LogFilter,
) -> anyhow::Result<()> {
    let lines = read_last_lines(path, tail, filter).await?;
    for line in lines {
        print_log_line(prefix, &line);
    }

    if !follow {
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
            if !filter.matches(clean) {
                continue;
            }
            print_log_line(prefix, clean);
        }
        offset = reader.seek(std::io::SeekFrom::Current(0)).await?;
    }
}

async fn follow_multiple_files(
    files: &[(String, PathBuf)],
    filter: &LogFilter,
) -> anyhow::Result<()> {
    let mut offsets: HashMap<String, u64> = HashMap::new();
    for (name, path) in files {
        let len = fs::metadata(path).await?.len();
        offsets.insert(name.clone(), len);
    }

    let mut tick = time::interval(Duration::from_millis(250));
    loop {
        tick.tick().await;
        for (name, path) in files {
            let metadata = fs::metadata(path).await?;
            let len = metadata.len();
            let mut offset = *offsets.get(name).unwrap_or(&0);
            if len < offset {
                offset = 0;
            }
            if len == offset {
                offsets.insert(name.clone(), offset);
                continue;
            }

            let file = fs::File::open(path).await?;
            let mut reader = BufReader::new(file);
            reader.seek(std::io::SeekFrom::Start(offset)).await?;

            let mut line = String::new();
            loop {
                line.clear();
                let read = reader.read_line(&mut line).await?;
                if read == 0 {
                    break;
                }
                let clean = line.trim_end_matches(['\n', '\r']);
                if !filter.matches(clean) {
                    continue;
                }
                print_log_line(Some(name), clean);
            }
            offsets.insert(
                name.clone(),
                reader.seek(std::io::SeekFrom::Current(0)).await?,
            );
        }
    }
}

async fn run_get_modes(
    all: bool,
    name: Option<String>,
    output: OutputFormat,
) -> anyhow::Result<()> {
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

    match output {
        OutputFormat::Table => print_modes_table(&rows),
        OutputFormat::Json => print_modes_json(&rows)?,
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

async fn run_logs(
    mode: Option<String>,
    mode_name: Option<String>,
    id: Option<Uuid>,
    all_modes: bool,
    tail: usize,
    follow: bool,
    since: Option<String>,
    before: Option<String>,
    filter: Option<String>,
    level: Option<String>,
) -> anyhow::Result<()> {
    let runtime_cfg = load_runtime_config().await?;
    let logs_dir = PathBuf::from(&runtime_cfg.logging.file_path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("logs"));
    let line_filter = LogFilter::new(since, before, filter, level)?;

    if all_modes {
        let config_modes = load_mode_config().await?;
        let mut files = Vec::new();
        for m in config_modes {
            let mode_id = mode_id_from_name(&m.name);
            if let Some(path) = find_mode_log_path(&logs_dir, mode_id.0).await {
                files.push((m.name, path));
            }
        }
        if files.is_empty() {
            anyhow::bail!("No per-mode log files found in {}", logs_dir.display());
        }

        for (name, path) in &files {
            let lines = read_last_lines(path, tail, &line_filter).await?;
            for line in lines {
                print_log_line(Some(name), &line);
            }
        }

        if follow {
            follow_multiple_files(&files, &line_filter).await?;
        }
        return Ok(());
    }

    let selected_name = mode_name.or(mode);
    let path = if let Some(id) = id {
        find_mode_log_path(&logs_dir, id).await
    } else if let Some(name) = selected_name {
        let mode_id = mode_id_from_name(&name);
        find_mode_log_path(&logs_dir, mode_id.0).await
    } else {
        Some(logs_dir.join("default.log"))
    };

    let Some(path) = path else {
        anyhow::bail!(
            "Log file not found. Expected one of mode-id naming variants under {}",
            logs_dir.display()
        );
    };

    if !path.exists() {
        anyhow::bail!("Log file not found: {}", path.display());
    }

    print_and_follow_file(&path, tail, follow, None, &line_filter).await
}

fn sanitize_filename(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn mode_log_file_candidates(id: Uuid) -> Vec<String> {
    let bare = format!("{id}.log");
    let wrapped = format!(
        "{}.log",
        sanitize_filename(&format!("AutonomyModeId({id})"))
    );
    if bare == wrapped {
        vec![bare]
    } else {
        vec![bare, wrapped]
    }
}

async fn find_mode_log_path(logs_dir: &Path, id: Uuid) -> Option<PathBuf> {
    for file in mode_log_file_candidates(id) {
        let path = logs_dir.join(file);
        if fs::try_exists(&path).await.unwrap_or(false) {
            return Some(path);
        }
    }
    None
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
            if let Ok(payload) = serde_json::from_str::<Value>(&json) {
                return Ok(SafectlIngress::Telemetry {
                    telemetry: TelemetryFrame::new(payload),
                });
            }
            anyhow::bail!(
                "Invalid telemetry payload. Provide full ingress JSON, a TelemetryFrame JSON, or a JSON payload object."
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

    let message = if let Some(m) = helper {
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

    let mut stream = UnixStream::connect(&sock_path).await?;
    let wire = serde_json::to_string(&message)?;
    stream.write_all(wire.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;
    println!("sent");
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
        }) => {
            run_logs(
                mode, mode_name, id, all_modes, tail, follow, since, before, filter, level,
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
        },
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
    fn parse_line_timestamp_rfc3339() {
        let line = "2026-05-06T21:00:43.773201Z INFO safe::x: hello";
        assert!(parse_line_timestamp(line).is_some());
    }

    #[test]
    fn log_filter_level_and_substring() {
        let filter = LogFilter::new(
            None,
            None,
            Some("hello".to_string()),
            Some("info".to_string()),
        )
        .unwrap();
        assert!(filter.matches("2026-01-01T00:00:00Z INFO target: hello world"));
        assert!(!filter.matches("2026-01-01T00:00:00Z ERROR target: hello world"));
    }

    #[test]
    fn colorize_info_line_adds_green_ansi() {
        let line = "2026-01-01T00:00:00Z INFO target: hello";
        let rendered = colorize_log_line(line);
        assert!(rendered.starts_with(ANSI_INFO));
        assert!(rendered.ends_with(ANSI_RESET));
    }

    #[test]
    fn colorize_non_level_line_is_unchanged() {
        let line = "plain text";
        let rendered = colorize_log_line(line);
        assert_eq!(rendered, line);
    }

    #[test]
    fn mode_log_candidates_include_wrapped_id_variant() {
        let id = Uuid::parse_str("123e4567-e89b-12d3-a456-426614174000").unwrap();
        let candidates = mode_log_file_candidates(id);
        assert!(
            candidates
                .iter()
                .any(|c| c == "123e4567-e89b-12d3-a456-426614174000.log")
        );
        assert!(
            candidates
                .iter()
                .any(|c| { c == "AutonomyModeId_123e4567-e89b-12d3-a456-426614174000_.log" })
        );
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
                    active: false,
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
                    active: false,
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
}
