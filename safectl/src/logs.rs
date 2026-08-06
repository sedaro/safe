use std::collections::{HashMap, VecDeque};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Context;
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use clap::ValueEnum;
use safe::runtime::{DEFAULT_LOG_STREAM, LogRecord, mode_id_from_name};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::time::{self, Duration};
use uuid::Uuid;

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_TRACE: &str = "\x1b[90m";
const ANSI_DEBUG: &str = "\x1b[34m";
const ANSI_INFO: &str = "\x1b[32m";
const ANSI_WARN: &str = "\x1b[33m";
const ANSI_ERROR: &str = "\x1b[31m";

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum LogOutputFormat {
    Text,
    Json,
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
        Ok(Self {
            since: since.map(|value| parse_timestamp(&value)).transpose()?,
            before: before.map(|value| parse_timestamp(&value)).transpose()?,
            filter,
            level: level.map(|value| value.to_uppercase()),
        })
    }

    fn matches(&self, entry: &LogEntry) -> bool {
        if let Some(level) = &self.level
            && entry.record.level.to_uppercase() != *level
        {
            return false;
        }

        if let Some(filter) = &self.filter {
            let mode_name = entry.mode_name.as_deref().unwrap_or_default();
            let fields = serde_json::to_string(&entry.record.fields).unwrap_or_default();
            let mode_id = entry
                .record
                .mode_id
                .map(|id| id.to_string())
                .unwrap_or_default();
            let searchable = [
                entry.record.message.as_str(),
                entry.record.target.as_str(),
                entry.record.stream.as_str(),
                mode_name,
                fields.as_str(),
                entry.record.level.as_str(),
                entry.record.timestamp.as_str(),
                mode_id.as_str(),
            ];
            if !searchable.iter().any(|value| value.contains(filter)) {
                return false;
            }
        }

        if self.since.is_none() && self.before.is_none() {
            return true;
        }

        let Some(timestamp) = entry.timestamp else {
            return false;
        };

        if self.since.is_some_and(|since| timestamp < since) {
            return false;
        }
        if self.before.is_some_and(|before| timestamp > before) {
            return false;
        }
        true
    }
}

#[derive(Debug, Clone)]
struct LogSource {
    mode_id: Option<Uuid>,
    mode_name: Option<String>,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct LogEntry {
    record: LogRecord,
    timestamp: Option<DateTime<Utc>>,
    mode_name: Option<String>,
    path: PathBuf,
    offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSignature {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

fn signature(metadata: &std::fs::Metadata) -> FileSignature {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        FileSignature {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    #[cfg(not(unix))]
    {
        FileSignature {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }
}

fn replaced(old: &FileSignature, new: &FileSignature) -> bool {
    #[cfg(unix)]
    {
        old.device != new.device || old.inode != new.inode
    }

    #[cfg(not(unix))]
    {
        old.len == new.len && old.modified != new.modified
    }
}

#[derive(Debug, Clone)]
struct FollowState {
    offset: u64,
    signature: Option<FileSignature>,
}

pub(crate) async fn run_logs(
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
    output: LogOutputFormat,
) -> anyhow::Result<()> {
    let runtime_cfg = super::load_runtime_config().await?;
    let logs_dir = PathBuf::from(&runtime_cfg.logging.file_path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("logs"));
    let with_target = runtime_cfg.tracing.with_target;
    let line_filter = LogFilter::new(since, before, filter, level)?;

    let sources = if all_modes {
        let config_modes = super::load_mode_config().await?;
        let mut sources = Vec::new();
        for mode in config_modes {
            let mode_id = mode_id_from_name(&mode.name).0;
            sources.extend(mode_log_sources(&logs_dir, mode_id, Some(mode.name)));
        }
        sources
    } else if let Some(mode_id) = id {
        mode_log_sources(&logs_dir, mode_id, None)
    } else if let Some(name) = mode_name.or(mode) {
        let mode_id = mode_id_from_name(&name).0;
        mode_log_sources(&logs_dir, mode_id, Some(name))
    } else {
        vec![LogSource {
            mode_id: None,
            mode_name: None,
            path: logs_dir.join("default.log"),
        }]
    };

    if !sources.iter().any(|source| source.path.exists()) {
        if all_modes {
            anyhow::bail!("No per-mode log files found in {}", logs_dir.display());
        }
        anyhow::bail!("Log file not found under {}", logs_dir.display());
    }

    let (mut entries, states) =
        read_initial_entries(&sources, &line_filter, tail, with_target).await?;
    sort_entries(&mut entries);
    let entries = tail_entries(entries, tail);
    let color = output == LogOutputFormat::Text && std::io::stdout().is_terminal();
    for entry in &entries {
        print_entry(entry, output, color)?;
    }

    if follow {
        follow_sources(sources, states, line_filter, output, color, with_target).await?;
    }

    Ok(())
}

fn mode_log_sources(logs_dir: &Path, mode_id: Uuid, mode_name: Option<String>) -> Vec<LogSource> {
    mode_log_file_candidates(mode_id)
        .into_iter()
        .map(|file| LogSource {
            mode_id: Some(mode_id),
            mode_name: mode_name.clone(),
            path: logs_dir.join(file),
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

async fn read_initial_entries(
    sources: &[LogSource],
    filter: &LogFilter,
    tail: usize,
    with_target: bool,
) -> anyhow::Result<(Vec<LogEntry>, HashMap<PathBuf, FollowState>)> {
    let mut entries = Vec::new();
    let mut states = HashMap::new();
    for source in sources {
        if states.contains_key(&source.path) {
            continue;
        }
        let (source_entries, state) =
            read_source_delta(source, 0, None, filter, Some(tail), with_target).await?;
        entries.extend(source_entries);
        states.insert(source.path.clone(), state);
    }
    Ok((entries, states))
}

async fn read_source_delta(
    source: &LogSource,
    requested_offset: u64,
    previous_signature: Option<FileSignature>,
    filter: &LogFilter,
    max_entries: Option<usize>,
    with_target: bool,
) -> anyhow::Result<(Vec<LogEntry>, FollowState)> {
    let metadata = match fs::metadata(&source.path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((
                Vec::new(),
                FollowState {
                    offset: 0,
                    signature: None,
                },
            ));
        }
        Err(error) => return Err(error.into()),
    };
    let current_signature = signature(&metadata);
    let truncated = previous_signature
        .as_ref()
        .is_some_and(|previous| current_signature.len < previous.len);
    let offset = if previous_signature
        .as_ref()
        .is_some_and(|previous| replaced(previous, &current_signature))
        || truncated
        || requested_offset > current_signature.len
    {
        0
    } else {
        requested_offset
    };
    let file = fs::File::open(&source.path).await?;
    let mut reader = BufReader::new(file);
    reader.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut entries = VecDeque::new();
    let mut next_offset = offset;
    loop {
        let line_offset = next_offset;
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 || !line.ends_with('\n') {
            break;
        }
        let line = line.trim_end_matches(['\n', '\r']);
        if let Some(entry) = parse_log_line(source, line, line_offset, with_target) {
            if filter.matches(&entry) {
                entries.push_back(entry);
                if max_entries.is_some_and(|max_entries| entries.len() > max_entries) {
                    entries.pop_front();
                }
            }
        }
        next_offset += bytes_read as u64;
    }

    Ok((
        entries.into_iter().collect(),
        FollowState {
            offset: next_offset,
            signature: Some(current_signature),
        },
    ))
}

fn parse_log_line(
    source: &LogSource,
    line: &str,
    offset: u64,
    with_target: bool,
) -> Option<LogEntry> {
    if line.trim().is_empty() {
        return None;
    }

    let mut record = serde_json::from_str::<LogRecord>(line)
        .ok()
        .or_else(|| parse_legacy_log_line(line, source.mode_id, with_target))?;
    if record.mode_id.is_none() {
        record.mode_id = source.mode_id;
    }
    let timestamp = parse_record_timestamp(&record.timestamp);
    Some(LogEntry {
        record,
        timestamp,
        mode_name: source.mode_name.clone(),
        path: source.path.clone(),
        offset,
    })
}

fn parse_record_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn parse_legacy_log_line(
    line: &str,
    mode_id: Option<Uuid>,
    with_target: bool,
) -> Option<LogRecord> {
    let (timestamp, rest) = split_legacy_timestamp(line);
    let (level, rest) = rest.split_once(char::is_whitespace)?;
    let rest = rest.trim_start();
    let (target, message) = if with_target {
        let mut parts = rest.split_whitespace();
        let next = parts.next().unwrap_or_default();
        let message_start = rest.find(next).unwrap_or(rest.len());
        let message = rest[message_start + next.len()..].trim();
        if message.is_empty() {
            (String::new(), next.to_string())
        } else {
            (next.to_string(), message.to_string())
        }
    } else {
        (String::new(), rest.to_string())
    };
    let (stream, message) = if let Some(message) = message.strip_prefix("stdout: ") {
        ("stdout".to_string(), message.to_string())
    } else if let Some(message) = message.strip_prefix("stderr: ") {
        ("stderr".to_string(), message.to_string())
    } else {
        (DEFAULT_LOG_STREAM.to_string(), message)
    };

    Some(LogRecord {
        timestamp: timestamp
            .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::Millis, true))
            .unwrap_or_default(),
        level: level.to_uppercase(),
        target,
        mode_id,
        stream,
        message,
        fields: Default::default(),
    })
}

fn split_legacy_timestamp(line: &str) -> (Option<DateTime<Utc>>, &str) {
    if line.starts_with("SystemTime {")
        && let Some(end) = line.find("} ")
    {
        let timestamp = parse_system_time_debug(&line[..=end]);
        return (timestamp, &line[end + 2..]);
    }

    let Some((timestamp, rest)) = line.split_once(char::is_whitespace) else {
        return (None, line);
    };
    if let Some(timestamp) = parse_record_timestamp(timestamp) {
        (Some(timestamp), rest.trim_start())
    } else {
        (None, line)
    }
}

fn parse_system_time_debug(value: &str) -> Option<DateTime<Utc>> {
    let seconds = parse_debug_number(value, "tv_sec: ")?;
    let nanos = parse_debug_number(value, "tv_nsec: ")?;
    Utc.timestamp_opt(seconds, nanos as u32).single()
}

fn parse_debug_number(value: &str, marker: &str) -> Option<i64> {
    let start = value.find(marker)? + marker.len();
    let value = &value[start..];
    let end = value
        .find(|character: char| !character.is_ascii_digit() && character != '-')
        .unwrap_or(value.len());
    value[..end].parse().ok()
}

fn sort_entries(entries: &mut [LogEntry]) {
    entries.sort_by(|left, right| {
        match (left.timestamp, right.timestamp) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| left.path.cmp(&right.path))
        .then_with(|| left.offset.cmp(&right.offset))
    });
}

fn tail_entries(entries: Vec<LogEntry>, tail: usize) -> Vec<LogEntry> {
    let start = entries.len().saturating_sub(tail);
    entries.into_iter().skip(start).collect()
}

async fn follow_sources(
    sources: Vec<LogSource>,
    mut states: HashMap<PathBuf, FollowState>,
    filter: LogFilter,
    output: LogOutputFormat,
    color: bool,
    with_target: bool,
) -> anyhow::Result<()> {
    let mut tick = time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let mut entries = Vec::new();
                for source in &sources {
                    let state = states.get(&source.path).cloned().unwrap_or(FollowState {
                        offset: 0,
                        signature: None,
                    });
                    let (source_entries, next_state) = read_source_delta(
                        source,
                        state.offset,
                        state.signature,
                        &filter,
                        None,
                        with_target,
                    ).await?;
                    entries.extend(source_entries);
                    states.insert(source.path.clone(), next_state);
                }
                sort_entries(&mut entries);
                for entry in &entries {
                    print_entry(entry, output, color)?;
                }
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    Ok(())
}

fn print_entry(entry: &LogEntry, output: LogOutputFormat, color: bool) -> anyhow::Result<()> {
    match output {
        LogOutputFormat::Text => println!("{}", format_text_entry(entry, color)),
        LogOutputFormat::Json => println!("{}", serde_json::to_string(&JsonLogEntry::from(entry))?),
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct JsonLogEntry {
    #[serde(flatten)]
    record: LogRecord,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode_name: Option<String>,
}

impl From<&LogEntry> for JsonLogEntry {
    fn from(entry: &LogEntry) -> Self {
        Self {
            record: entry.record.clone(),
            mode_name: entry.mode_name.clone(),
        }
    }
}

fn format_text_entry(entry: &LogEntry, color: bool) -> String {
    let timestamp = if entry.record.timestamp.is_empty() {
        "-".to_string()
    } else {
        sanitize_text(&entry.record.timestamp)
    };
    let level = format_level(&entry.record.level, color);
    let scope = entry
        .mode_name
        .clone()
        .or_else(|| entry.record.mode_id.map(|id| id.to_string()))
        .unwrap_or_else(|| "safe".to_string());
    let target = if entry.record.target.is_empty() {
        "-".to_string()
    } else {
        sanitize_text(&entry.record.target)
    };
    let fields = entry
        .record
        .fields
        .iter()
        .map(|(name, value)| {
            format!(
                "{}={}",
                sanitize_text(name),
                sanitize_text(&serde_json::to_string(value).unwrap_or_default())
            )
        })
        .collect::<Vec<_>>();
    let fields = if fields.is_empty() {
        String::new()
    } else {
        format!(" {}", fields.join(" "))
    };
    format!(
        "{timestamp} {level} [{}] {:<7} {target}: {}{}",
        sanitize_text(&scope),
        sanitize_text(&entry.record.stream),
        sanitize_text(&entry.record.message),
        fields,
    )
}

fn format_level(level: &str, color: bool) -> String {
    let level = format!("{:<5}", sanitize_text(level).to_uppercase());
    if !color {
        return level;
    }
    let ansi = match level.trim() {
        "TRACE" => ANSI_TRACE,
        "DEBUG" => ANSI_DEBUG,
        "INFO" => ANSI_INFO,
        "WARN" => ANSI_WARN,
        "ERROR" => ANSI_ERROR,
        _ => return level,
    };
    format!("{ansi}{level}{ANSI_RESET}")
}

fn sanitize_text(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                output.push_str(&format!("\\u{{{:x}}}", character as u32));
            }
            character => output.push(character),
        }
    }
    output
}

fn sanitize_filename(input: &str) -> String {
    input
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn parse_timestamp(value: &str) -> anyhow::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .with_context(|| format!("Invalid timestamp `{value}` (expected RFC3339)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(mode_id: Option<Uuid>) -> LogSource {
        LogSource {
            mode_id,
            mode_name: Some("ExampleMode".to_string()),
            path: PathBuf::from("example.log"),
        }
    }

    #[test]
    fn parses_structured_record() {
        let mode_id = Uuid::from_u128(1);
        let line = serde_json::json!({
            "timestamp": "2026-08-06T12:00:00.123Z",
            "level": "WARN",
            "target": "safe::mode",
            "mode_id": mode_id,
            "stream": "stderr",
            "message": "failed",
            "fields": {"attempt": 2}
        })
        .to_string();
        let entry = parse_log_line(&source(Some(mode_id)), &line, 0, true).unwrap();
        assert_eq!(entry.record.mode_id, Some(mode_id));
        assert_eq!(entry.record.stream, "stderr");
        assert_eq!(entry.record.fields["attempt"], 2);
    }

    #[test]
    fn parses_legacy_system_time_record() {
        let line =
            "SystemTime { tv_sec: 1762434043, tv_nsec: 123000000 } INFO safe::mode stdout: hello";
        let entry = parse_log_line(&source(Some(Uuid::from_u128(1))), line, 0, true).unwrap();
        assert_eq!(entry.record.level, "INFO");
        assert_eq!(entry.record.stream, "stdout");
        assert_eq!(entry.record.message, "hello");
        assert!(entry.timestamp.is_some());
    }

    #[test]
    fn parses_legacy_record_without_target() {
        let line = "SystemTime { tv_sec: 1762434043, tv_nsec: 123000000 } WARN stderr: failed";
        let entry = parse_log_line(&source(None), line, 0, false).unwrap();
        assert_eq!(entry.record.target, "");
        assert_eq!(entry.record.stream, "stderr");
        assert_eq!(entry.record.message, "failed");
    }

    #[test]
    fn time_filter_excludes_unknown_records() {
        let filter =
            LogFilter::new(Some("2026-01-01T00:00:00Z".to_string()), None, None, None).unwrap();
        let entry = LogEntry {
            record: LogRecord {
                timestamp: String::new(),
                level: "INFO".to_string(),
                target: String::new(),
                mode_id: None,
                stream: DEFAULT_LOG_STREAM.to_string(),
                message: "unknown".to_string(),
                fields: Default::default(),
            },
            timestamp: None,
            mode_name: None,
            path: PathBuf::from("default.log"),
            offset: 0,
        };
        assert!(!filter.matches(&entry));
    }

    #[test]
    fn all_mode_entries_sort_by_timestamp() {
        let mut entries = vec![
            LogEntry {
                record: LogRecord {
                    timestamp: "2026-01-01T00:00:02Z".to_string(),
                    level: "INFO".to_string(),
                    target: String::new(),
                    mode_id: None,
                    stream: DEFAULT_LOG_STREAM.to_string(),
                    message: "second".to_string(),
                    fields: Default::default(),
                },
                timestamp: parse_record_timestamp("2026-01-01T00:00:02Z"),
                mode_name: Some("B".to_string()),
                path: PathBuf::from("b.log"),
                offset: 0,
            },
            LogEntry {
                record: LogRecord {
                    timestamp: "2026-01-01T00:00:01Z".to_string(),
                    level: "INFO".to_string(),
                    target: String::new(),
                    mode_id: None,
                    stream: DEFAULT_LOG_STREAM.to_string(),
                    message: "first".to_string(),
                    fields: Default::default(),
                },
                timestamp: parse_record_timestamp("2026-01-01T00:00:01Z"),
                mode_name: Some("A".to_string()),
                path: PathBuf::from("a.log"),
                offset: 0,
            },
        ];
        sort_entries(&mut entries);
        assert_eq!(entries[0].record.message, "first");
        assert_eq!(entries[1].record.message, "second");
    }

    #[test]
    fn text_output_escapes_control_characters() {
        let entry = LogEntry {
            record: LogRecord {
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                level: "INFO".to_string(),
                target: "target".to_string(),
                mode_id: None,
                stream: DEFAULT_LOG_STREAM.to_string(),
                message: "hello\nworld\u{1b}[2J".to_string(),
                fields: Default::default(),
            },
            timestamp: parse_record_timestamp("2026-01-01T00:00:00Z"),
            mode_name: None,
            path: PathBuf::from("default.log"),
            offset: 0,
        };
        let rendered = format_text_entry(&entry, false);
        assert!(rendered.contains("hello\\nworld\\u{1b}[2J"));
    }

    #[test]
    fn mode_candidates_keep_bare_and_legacy_names() {
        let candidates = mode_log_file_candidates(Uuid::from_u128(1));
        assert_eq!(candidates.len(), 2);
        assert!(candidates[0].ends_with(".log"));
        assert!(candidates[1].contains("AutonomyModeId_"));
    }
}
