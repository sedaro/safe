use std::collections::HashSet;
use std::fmt::{Debug, Write as _};

use chrono::Utc;
use comfy_table::{Attribute, Cell, CellAlignment, ColumnConstraint, ContentArrangement, Table};
use comfy_table::{Width, presets::UTF8_FULL};
use crossterm::terminal;
use safe::protocol::{Command, TimedCommand};
use safe::runtime::{
    BoardCommandState, BoardCommandStatus, HostCommandStatus, OperationalStatus,
    ProcessResourceSnapshot,
};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{ModeDescribeView, ModeTopRow, ModeView, TelemetryView};

const MISSING: &str = "-";

pub(crate) fn render_modes_table(modes: &[ModeView]) -> String {
    let rows = modes
        .iter()
        .map(|mode| {
            let selection = if mode.active {
                mode.selection_reason.as_deref().unwrap_or("selected")
            } else {
                MISSING
            };

            vec![
                mode.name.clone(),
                bool_label(mode.active),
                optional_bool(mode.enabled),
                optional_bool(mode.eligible),
                mode.priority
                    .map(|priority| priority.to_string())
                    .unwrap_or_else(|| MISSING.to_string()),
                mode_state_label(mode.connection.as_ref()),
                mode_state_label(mode.handler.as_ref()),
                selection.to_string(),
                mode.detail.as_deref().unwrap_or(MISSING).to_string(),
            ]
        })
        .collect();

    render_table(
        Some(&[
            "NAME",
            "ACTIVE",
            "ENABLED",
            "ELIGIBLE",
            "PRIORITY",
            "CONNECTION",
            "HANDLER",
            "SELECTION",
            "DETAIL",
        ]),
        rows,
        &[28, 9, 9, 9, 9, 16, 15, 28, 36],
        &[1, 2, 3, 4],
    )
}

pub(crate) fn render_mode_describe_table(view: &ModeDescribeView) -> String {
    let scalar_table = render_table(
        Some(&["FIELD", "VALUE"]),
        vec![
            vec!["Name".to_string(), view.name.clone()],
            vec!["ID".to_string(), view.id.to_string()],
            vec!["Active".to_string(), bool_label(view.active)],
            vec!["Enabled".to_string(), bool_label(view.enabled)],
            vec!["Priority".to_string(), view.priority.to_string()],
        ],
        &[18, 80],
        &[],
    );

    format!(
        "{scalar_table}\n\nActivation\n{}\n\nMode config\n{}",
        render_json_block(view.activation.as_ref()),
        indent_block(&render_json_pretty(&view.mode_config)),
    )
}

pub(crate) fn render_top_table(rows: &[ModeTopRow]) -> String {
    let rows = rows.iter().map(top_row_values).collect();
    render_table(
        Some(&top_headers()),
        rows,
        &[32, 9, 10, 14, 14, 14, 8, 10],
        &[2, 3, 4, 5, 6],
    )
}

pub(crate) fn top_headers() -> [&'static str; 8] {
    [
        "MODE",
        "ACTIVE",
        "CPU %",
        "MEMORY",
        "DISK READ",
        "DISK WRITE",
        "PROCS",
        "AGE",
    ]
}

pub(crate) fn top_row_values(row: &ModeTopRow) -> Vec<String> {
    let Some(snapshot) = &row.snapshot else {
        return vec![
            row.mode.name.clone(),
            bool_label(row.mode.active),
            MISSING.to_string(),
            MISSING.to_string(),
            MISSING.to_string(),
            MISSING.to_string(),
            MISSING.to_string(),
            MISSING.to_string(),
        ];
    };

    vec![
        row.mode.name.clone(),
        bool_label(row.mode.active),
        format_cpu(snapshot.cpu_percent),
        format_bytes(snapshot.memory_bytes),
        format_bytes(snapshot.disk_read_bytes),
        format_bytes(snapshot.disk_written_bytes),
        snapshot.process_count.to_string(),
        format_age_secs(snapshot.timestamp_unix_ms),
    ]
}

pub(crate) fn render_telemetry_table(views: &[TelemetryView]) -> String {
    render_telemetry(views)
}

fn render_telemetry(views: &[TelemetryView]) -> String {
    let rows = views
        .iter()
        .map(|view| {
            vec![
                view.seq
                    .map(|seq| seq.to_string())
                    .unwrap_or_else(|| MISSING.to_string()),
                view.source.as_deref().unwrap_or(MISSING).to_string(),
                view.ts_mono.to_string(),
                payload_preview(&view.payload),
            ]
        })
        .collect();

    render_table_no_wrap(
        Some(&["SEQ", "SOURCE", "TS MONO", "PAYLOAD"]),
        rows,
        &telemetry_widths(),
        &[0, 2],
    )
}

fn telemetry_widths() -> Vec<u16> {
    let mut widths = vec![10usize, 24, 14, 72];
    let minimums = [5usize, 10, 7, 8];
    let Some((terminal_width, _)) = terminal::size().ok() else {
        return widths.into_iter().map(|width| width as u16).collect();
    };

    let available = usize::from(terminal_width).saturating_sub(3 * widths.len() + 1);
    let mut excess = widths.iter().sum::<usize>().saturating_sub(available);
    for index in [3, 1, 2, 0] {
        let reducible = widths[index].saturating_sub(minimums[index]);
        let reduction = reducible.min(excess);
        widths[index] -= reduction;
        excess -= reduction;
    }

    widths.into_iter().map(|width| width as u16).collect()
}

pub(crate) fn render_board_table(entries: &[BoardCommandStatus]) -> String {
    let rows = entries
        .iter()
        .map(|entry| {
            vec![
                entry.id.0.clone(),
                board_state_label(&entry.state).to_string(),
                entry.from.to_string(),
                timed_command_label(&entry.command),
                entry
                    .decision_reason
                    .as_deref()
                    .unwrap_or(MISSING)
                    .to_string(),
            ]
        })
        .collect();

    render_table(
        Some(&["ID", "STATE", "FROM", "COMMAND", "REASON"]),
        rows,
        &[44, 12, 38, 42, 40],
        &[],
    )
}

pub(crate) fn render_request_table(statuses: &[HostCommandStatus]) -> String {
    let rows = statuses
        .iter()
        .map(|status| {
            vec![
                pretty_debug_label(&status.state),
                status.ts_mono.to_string(),
                status.detail.clone(),
            ]
        })
        .collect();

    render_table(
        Some(&["STATE", "TS MONO", "DETAIL"]),
        rows,
        &[18, 14, 80],
        &[1],
    )
}

pub(crate) fn render_status_table(
    status: &OperationalStatus,
    process_alive: bool,
    modes: &[ModeView],
) -> String {
    let daemon = if process_alive && status.daemon.running && !status.daemon.halted {
        "RUNNING"
    } else {
        "STOPPED"
    };

    let telemetry = match &status.telemetry.latest {
        Some(latest) => format!(
            "{} received | last {} | source {} | ts_mono {}",
            status.telemetry.received_count,
            status
                .telemetry
                .last_received_at
                .as_deref()
                .unwrap_or("unknown"),
            latest.source.as_deref().unwrap_or("unknown"),
            latest.ts_mono,
        ),
        None => format!("{} received", status.telemetry.received_count),
    };

    let board_count = |state: &BoardCommandState| {
        status
            .board
            .iter()
            .filter(|entry| std::mem::discriminant(&entry.state) == std::mem::discriminant(state))
            .count()
    };
    let board = format!(
        "{} pending | {} approved | {} published | {} rejected",
        board_count(&BoardCommandState::Pending),
        board_count(&BoardCommandState::Approved),
        board_count(&BoardCommandState::Published),
        board_count(&BoardCommandState::Rejected),
    );

    let mut summary_rows = vec![
        vec![
            "SAFE".to_string(),
            format!(
                "{daemon} | pid {} | snapshot {}",
                status.daemon.pid, status.updated_at
            ),
        ],
        vec!["TELEMETRY".to_string(), telemetry],
        vec!["BOARD".to_string(), board],
        vec![
            "LAST EVENT".to_string(),
            status.daemon.last_seq_applied.to_string(),
        ],
    ];
    if let Some(fault) = &status.daemon.fault {
        summary_rows.push(vec!["FAULT".to_string(), fault.clone()]);
    }

    format!(
        "{}\n\n{}",
        render_table(Some(&["FIELD", "VALUE"]), summary_rows, &[16, 100], &[]),
        render_modes_table(modes)
    )
}

pub(crate) fn format_age_secs(timestamp_unix_ms: u64) -> String {
    let now_ms = Utc::now().timestamp_millis().max(0) as u64;
    if now_ms <= timestamp_unix_ms {
        return "now".to_string();
    }

    let seconds = (now_ms - timestamp_unix_ms) / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 60 * 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else if seconds < 24 * 60 * 60 {
        format!("{}h {}m", seconds / (60 * 60), (seconds / 60) % 60)
    } else {
        format!(
            "{}d {}h",
            seconds / (24 * 60 * 60),
            (seconds / (60 * 60)) % 24
        )
    }
}

pub(crate) fn payload_preview(payload: &Value) -> String {
    const LIMIT: usize = 72;

    let rendered = serde_json::to_string(payload).unwrap_or_else(|_| payload.to_string());
    truncate_display(&sanitize_cell(&rendered), LIMIT)
}

pub(crate) fn process_tree_rows(processes: &[ProcessResourceSnapshot]) -> Vec<Vec<String>> {
    let pid_set: HashSet<u32> = processes.iter().map(|process| process.pid).collect();
    let mut roots: Vec<usize> = processes
        .iter()
        .enumerate()
        .filter(|(_, process)| {
            process
                .parent_pid
                .is_none_or(|parent| !pid_set.contains(&parent))
        })
        .map(|(index, _)| index)
        .collect();

    if roots.is_empty() {
        roots = (0..processes.len()).collect();
    }
    roots.sort_by_key(|index| processes[*index].pid);

    let mut rows = Vec::new();
    let mut visited = HashSet::new();
    for index in roots {
        append_process_row(processes, index, 0, "process", "", &mut visited, &mut rows);
    }
    rows
}

fn append_process_row(
    processes: &[ProcessResourceSnapshot],
    index: usize,
    depth: usize,
    relation: &str,
    branch: &str,
    visited: &mut HashSet<u32>,
    rows: &mut Vec<Vec<String>>,
) {
    let process = &processes[index];
    if !visited.insert(process.pid) {
        return;
    }

    let command = process_command_label(&process.command);
    let name = if depth == 0 {
        command
    } else {
        format!(
            "{}{}{} ({})",
            "  ".repeat(depth),
            branch,
            command,
            process.pid
        )
    };
    let name = if depth == 0 {
        format!("{name} ({})", process.pid)
    } else {
        name
    };
    rows.push(vec![
        name,
        relation.to_string(),
        format_cpu(process.cpu_percent),
        format_bytes(process.memory_bytes),
        format_bytes(process.disk_read_bytes),
        format_bytes(process.disk_written_bytes),
        MISSING.to_string(),
        MISSING.to_string(),
    ]);

    let mut children: Vec<usize> = processes
        .iter()
        .enumerate()
        .filter(|(_, child)| child.parent_pid == Some(process.pid))
        .map(|(child_index, _)| child_index)
        .collect();
    children.sort_by_key(|child_index| processes[*child_index].pid);
    for (child_position, child_index) in children.iter().enumerate() {
        let branch = if child_position + 1 == children.len() {
            "└─ "
        } else {
            "├─ "
        };
        append_process_row(
            processes,
            *child_index,
            depth + 1,
            "child",
            branch,
            visited,
            rows,
        );
    }
}

fn render_table(
    headers: Option<&[&str]>,
    rows: Vec<Vec<String>>,
    max_widths: &[u16],
    numeric_columns: &[usize],
) -> String {
    render_table_with(
        headers,
        rows,
        max_widths,
        numeric_columns,
        ContentArrangement::Dynamic,
        false,
    )
}

fn render_table_no_wrap(
    headers: Option<&[&str]>,
    rows: Vec<Vec<String>>,
    max_widths: &[u16],
    numeric_columns: &[usize],
) -> String {
    render_table_with(
        headers,
        rows,
        max_widths,
        numeric_columns,
        ContentArrangement::Disabled,
        true,
    )
}

fn render_table_with(
    headers: Option<&[&str]>,
    rows: Vec<Vec<String>>,
    max_widths: &[u16],
    numeric_columns: &[usize],
    arrangement: ContentArrangement,
    truncate: bool,
) -> String {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(arrangement)
        .set_truncation_indicator("...");

    if let Some(headers) = headers {
        let header = headers
            .iter()
            .enumerate()
            .map(|(index, header)| {
                Cell::new(format_cell(header, index, max_widths, truncate))
                    .add_attribute(Attribute::Bold)
            })
            .collect::<Vec<_>>();
        table.set_header(header);
    }

    for row in rows {
        let cells = row
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                let cell = Cell::new(format_cell(&value, index, max_widths, truncate));
                if numeric_columns.contains(&index) {
                    cell.set_alignment(CellAlignment::Right)
                } else {
                    cell
                }
            })
            .collect::<Vec<_>>();
        table.add_row(cells);
    }

    if !max_widths.is_empty() {
        table.set_constraints(
            max_widths
                .iter()
                .copied()
                .map(|width| ColumnConstraint::UpperBoundary(Width::Fixed(width))),
        );
    }

    table.to_string()
}

fn format_cell(value: &str, index: usize, max_widths: &[u16], truncate: bool) -> String {
    let value = sanitize_cell(value);
    if truncate && let Some(max_width) = max_widths.get(index) {
        return truncate_display(&value, usize::from(*max_width));
    }
    value
}

fn render_json_block(value: Option<&Value>) -> String {
    match value {
        Some(value) => indent_block(&render_json_pretty(value)),
        None => "  (none)".to_string(),
    }
}

fn render_json_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn indent_block(value: &str) -> String {
    value
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn sanitize_cell(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => sanitized.push_str("\\n"),
            '\r' => sanitized.push_str("\\r"),
            '\t' => sanitized.push_str("\\t"),
            character if character.is_control() => {
                let _ = write!(sanitized, "\\x{:02x}", character as u32);
            }
            character => sanitized.push(character),
        }
    }
    sanitized
}

fn truncate_display(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_string();
    }

    const INDICATOR: &str = "...";
    let indicator_width = UnicodeWidthStr::width(INDICATOR);
    if max_width <= indicator_width {
        return take_display_width(INDICATOR, max_width);
    }

    format!(
        "{}{}",
        take_display_width(value, max_width - indicator_width),
        INDICATOR
    )
}

fn take_display_width(value: &str, max_width: usize) -> String {
    let mut result = String::new();
    let mut width = 0;
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > max_width {
            break;
        }
        result.push(character);
        width += character_width;
    }
    result
}

fn bool_label(value: bool) -> String {
    if value {
        "yes".to_string()
    } else {
        "no".to_string()
    }
}

fn optional_bool(value: Option<bool>) -> String {
    value.map(bool_label).unwrap_or_else(|| MISSING.to_string())
}

fn mode_state_label<T: Debug>(state: Option<&T>) -> String {
    state
        .map(pretty_debug_label)
        .unwrap_or_else(|| MISSING.to_string())
}

fn pretty_debug_label<T: Debug>(value: &T) -> String {
    let raw = format!("{value:?}");
    let mut label = String::new();
    let mut previous_was_lowercase = false;
    for character in raw.chars() {
        if character == '_' {
            if !label.ends_with(' ') {
                label.push(' ');
            }
            previous_was_lowercase = false;
            continue;
        }
        if character.is_uppercase() && previous_was_lowercase {
            label.push(' ');
        }
        for lowercase in character.to_lowercase() {
            label.push(lowercase);
        }
        previous_was_lowercase = character.is_lowercase();
    }
    label.to_uppercase()
}

fn format_cpu(cpu_percent: f64) -> String {
    format!("{cpu_percent:.1}%")
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn board_state_label(state: &BoardCommandState) -> &'static str {
    match state {
        BoardCommandState::Pending => "PENDING",
        BoardCommandState::Approved => "APPROVED",
        BoardCommandState::Rejected => "REJECTED",
        BoardCommandState::Published => "PUBLISHED",
    }
}

fn timed_command_label(command: &TimedCommand) -> String {
    match command {
        TimedCommand::Now(command) => format!("NOW: {}", command_label(command)),
        TimedCommand::NOOP => "NO-OP".to_string(),
        TimedCommand::Scheduled { cmd, gps_time } => {
            format!("SCHEDULED: {} @ GPS {gps_time:.3}", command_label(cmd))
        }
    }
}

fn command_label(command: &Command) -> String {
    command.into()
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

#[cfg(test)]
mod tests {
    use super::*;
    use safe::protocol::{BoardCmdId, Command, TimedCommand};
    use safe::runtime::{
        BoardCommandState, HostCommandStatusState, ModeConnectionState, ModeHandlerState,
        ModeResourceSnapshot, mode_id_from_name,
    };
    use uuid::Uuid;

    #[test]
    fn table_sanitizes_control_characters() {
        let rendered = render_table(
            Some(&["VALUE"]),
            vec![vec!["line\none\t\u{1b}".to_string()]],
            &[20],
            &[],
        );

        assert!(rendered.contains("line\\none\\t\\x1b"));
        assert!(!rendered.contains("line\none"));
    }

    #[test]
    fn labels_split_camel_case_states() {
        assert_eq!(
            pretty_debug_label(&ModeConnectionState::NotStarted),
            "NOT STARTED"
        );
        assert_eq!(pretty_debug_label(&ModeHandlerState::Faulted), "FAULTED");
    }

    #[test]
    fn timed_commands_are_operator_friendly() {
        assert_eq!(
            timed_command_label(&TimedCommand::Now(Command::PointNadir)),
            "NOW: PointNadir"
        );
        assert_eq!(timed_command_label(&TimedCommand::NOOP), "NO-OP");
    }

    #[test]
    fn payload_preview_respects_display_width() {
        let preview = payload_preview(&serde_json::json!({"value": "界".repeat(100)}));
        assert!(UnicodeWidthStr::width(preview.as_str()) <= 72);
        assert!(preview.ends_with("..."));
    }

    #[test]
    fn process_tree_terminates_on_cycles() {
        let processes = vec![
            ProcessResourceSnapshot {
                pid: 1,
                parent_pid: Some(2),
                command: "one".to_string(),
                cpu_percent: 1.0,
                memory_bytes: 1,
                disk_read_bytes: 1,
                disk_written_bytes: 1,
            },
            ProcessResourceSnapshot {
                pid: 2,
                parent_pid: Some(1),
                command: "two".to_string(),
                cpu_percent: 2.0,
                memory_bytes: 2,
                disk_read_bytes: 2,
                disk_written_bytes: 2,
            },
        ];

        let rows = process_tree_rows(&processes);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn mode_table_includes_detail_column() {
        let mode = ModeView {
            name: "Mode".to_string(),
            id: Uuid::from_u128(1),
            priority: Some(1),
            enabled: Some(true),
            eligible: Some(false),
            active: false,
            selection_reason: None,
            connection: Some(ModeConnectionState::NotStarted),
            handler: None,
            detail: Some("waiting for telemetry".to_string()),
        };

        let rendered = render_modes_table(&[mode]);
        assert!(rendered.contains("DETAIL"));
        assert!(rendered.contains("waiting for telemetry"));
        assert!(rendered.contains("NOT STARTED"));
    }

    #[test]
    fn describe_keeps_json_sections_readable() {
        let view = ModeDescribeView {
            name: "Mode".to_string(),
            id: Uuid::from_u128(1),
            active: true,
            enabled: true,
            priority: 2,
            activation: Some(serde_json::json!({"all": [{"value": 1}]})),
            mode_config: serde_json::json!({"threshold": 3}),
        };

        let rendered = render_mode_describe_table(&view);
        assert!(rendered.contains("FIELD"));
        assert!(rendered.contains("Activation\n  {"));
        assert!(rendered.contains("Mode config\n  {"));
        assert!(rendered.contains("\"threshold\": 3"));
    }

    #[test]
    fn top_table_uses_units_and_clear_headers() {
        let mode = ModeView {
            name: "Mode".to_string(),
            id: Uuid::from_u128(1),
            priority: Some(1),
            enabled: Some(true),
            eligible: Some(true),
            active: true,
            selection_reason: Some("highest priority".to_string()),
            connection: None,
            handler: None,
            detail: None,
        };
        let row = ModeTopRow {
            mode,
            snapshot: Some(ModeResourceSnapshot {
                mode_id: safe::runtime::mode_id_from_name("Mode"),
                timestamp_unix_ms: Utc::now().timestamp_millis().max(0) as u64,
                cpu_percent: 12.5,
                memory_bytes: 2048,
                disk_read_bytes: 4096,
                disk_written_bytes: 8192,
                process_count: 3,
                min_cpu_30_min: 0.0,
                max_cpu_30_min: 0.0,
                avg_cpu_30_min: 0.0,
                min_memory_30_min: 0,
                max_memory_30_min: 0,
                avg_memory_30_min: 0,
                processes: vec![],
            }),
        };

        let rendered = render_top_table(&[row]);
        assert!(rendered.contains("DISK READ"));
        assert!(rendered.contains("2.0 KiB"));
        assert!(rendered.contains("12.5%"));
    }

    #[test]
    fn telemetry_table_stays_single_line_per_row() {
        let view = TelemetryView {
            seq: Some(4),
            source: Some("sim".to_string()),
            ts_mono: 10,
            payload: serde_json::json!({"value": "x".repeat(300)}),
        };

        let rendered = render_telemetry_table(std::slice::from_ref(&view));
        assert!(rendered.contains("TS MONO"));
        assert_eq!(rendered.lines().count(), 5);
    }

    #[test]
    fn board_and_request_tables_use_safe_labels() {
        let board = BoardCommandStatus {
            id: BoardCmdId("1:mode:0".to_string()),
            from: mode_id_from_name("Mode"),
            command: TimedCommand::Scheduled {
                cmd: Command::PointNadir,
                gps_time: 123.456,
            },
            proposed_ts_mono: 1,
            state: BoardCommandState::Pending,
            decision_by: None,
            decision_reason: Some("operator\nreview".to_string()),
            decision_ts_mono: None,
        };
        let board_rendered = render_board_table(&[board]);
        assert!(board_rendered.contains("SCHEDULED: PointNadir @ GPS 123.456"));
        assert!(board_rendered.contains("operator\\nreview"));
        assert!(!board_rendered.contains("Scheduled {"));

        let request = HostCommandStatus {
            request_id: "request".to_string(),
            state: HostCommandStatusState::Received,
            detail: "accepted\tby operator".to_string(),
            ts_mono: 4,
        };
        let request_rendered = render_request_table(&[request]);
        assert!(request_rendered.contains("RECEIVED"));
        assert!(request_rendered.contains("accepted\\tby operator"));
    }
}
