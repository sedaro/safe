use std::path::Path;
use std::{
    collections::{BTreeMap, HashMap},
    fs::OpenOptions,
    io::{self, IsTerminal, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use chrono::{SecondsFormat, Utc};
use serde_json::Value;
use tracing::{Event, Subscriber};
#[cfg(feature = "env-filter")]
use tracing_subscriber::EnvFilter;
#[cfg(not(feature = "env-filter"))]
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};
use tracing_subscriber::{fmt::MakeWriter, registry::LookupSpan};
use uuid::Uuid;

use crate::config::Config;
use crate::runtime::{DEFAULT_LOG_STREAM, LogRecord};

tokio::task_local! {
    pub static CURRENT_TASK_ID: Uuid;
}

/// Keep this alive for non-blocking appenders if you add them later.
pub struct TelemetryGuards;

/// Initialize tracing with human-readable stdout and structured per-scope files.
pub fn init_tracing(cfg: &Config) -> Result<TelemetryGuards, Box<dyn std::error::Error>> {
    let base_path = PathBuf::from(&cfg.logging.file_path);
    let logs_dir = base_path
        .parent()
        .unwrap_or_else(|| Path::new("logs"))
        .to_path_buf();
    std::fs::create_dir_all(&logs_dir)?;

    let router = PerIdMakeWriter::new(logs_dir, "default.log");
    let per_id_layer = PerIdFileLayer::new(router, cfg.tracing.with_target);

    let stdout_layer = fmt::layer()
        .with_ansi(std::io::stdout().is_terminal())
        .with_target(cfg.tracing.with_target)
        .with_writer(std::io::stdout);

    #[cfg(feature = "env-filter")]
    {
        let filter = EnvFilter::try_new(cfg.tracing.filter.clone())
            .or_else(|_| EnvFilter::try_new(cfg.tracing.level.clone()))?;
        tracing_subscriber::registry()
            .with(filter)
            .with(stdout_layer)
            .with(per_id_layer)
            .init();
    }
    #[cfg(not(feature = "env-filter"))]
    {
        let filter = cfg
            .tracing
            .level
            .parse::<tracing::Level>()
            .map(LevelFilter::from_level)
            .unwrap_or(LevelFilter::INFO);
        tracing_subscriber::registry()
            .with(filter)
            .with(stdout_layer)
            .with(per_id_layer)
            .init();
    }

    tracing::info!("logging initialized");

    Ok(TelemetryGuards)
}

/// Routes events to `<mode-uuid>.log` if a mode ID exists, else `default.log`.
#[derive(Clone)]
pub struct PerIdMakeWriter {
    state: Arc<Mutex<PerIdState>>,
}

struct PerIdState {
    logs_dir: PathBuf,
    fallback_file: String,
    files: HashMap<String, std::fs::File>,
}

impl PerIdMakeWriter {
    fn new(logs_dir: PathBuf, fallback_file: impl Into<String>) -> Self {
        Self {
            state: Arc::new(Mutex::new(PerIdState {
                logs_dir,
                fallback_file: fallback_file.into(),
                files: HashMap::new(),
            })),
        }
    }

    fn writer_for_id(&self, id: Option<&str>) -> PerIdWriter {
        PerIdWriter {
            state: self.state.clone(),
            key: sanitize_filename(id.unwrap_or("default")),
        }
    }
}

impl<'a> MakeWriter<'a> for PerIdMakeWriter {
    type Writer = PerIdWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.writer_for_id(None)
    }
}

impl PerIdMakeWriter {
    fn write_line_for(&self, id: Option<&str>, line: &str) -> io::Result<()> {
        let mut writer = self.writer_for_id(id);
        writer.write_all(line.as_bytes())?;
        writer.flush()
    }
}

pub struct PerIdWriter {
    state: Arc<Mutex<PerIdState>>,
    key: String,
}

impl Write for PerIdWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.state.lock().expect("poisoned mutex");

        let file_name = if self.key == "default" {
            state.fallback_file.clone()
        } else {
            format!("{}.log", self.key)
        };

        if !state.files.contains_key(&file_name) {
            let path = state.logs_dir.join(&file_name);
            let file = OpenOptions::new().create(true).append(true).open(path)?;
            state.files.insert(file_name.clone(), file);
        }

        state
            .files
            .get_mut(&file_name)
            .expect("inserted file missing")
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self.state.lock().expect("poisoned mutex");

        let file_name = if self.key == "default" {
            state.fallback_file.clone()
        } else {
            format!("{}.log", self.key)
        };

        if let Some(file) = state.files.get_mut(&file_name) {
            file.flush()
        } else {
            Ok(())
        }
    }
}

/// A dedicated layer that writes each event to a structured per-scope file.
pub struct PerIdFileLayer {
    router: PerIdMakeWriter,
    with_target: bool,
}

impl PerIdFileLayer {
    pub fn new(router: PerIdMakeWriter, with_target: bool) -> Self {
        Self {
            router,
            with_target,
        }
    }
}

impl<S> tracing_subscriber::Layer<S> for PerIdFileLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let meta = event.metadata();

        let mut visitor = IdExtractVisitor::default();
        event.record(&mut visitor);
        if visitor.mode_id.is_none() {
            if let Some(scope) = ctx.event_scope(event) {
                for span in scope.from_root() {
                    if let Some(mode_id) = span.extensions().get::<CapturedModeId>() {
                        visitor.mode_id = Some(mode_id.0.clone());
                        break;
                    }
                }
            }
        }

        let mode_id = visitor
            .mode_id
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok());
        let record = LogRecord {
            timestamp: chrono_like_now(),
            level: meta.level().to_string(),
            target: if self.with_target {
                meta.target().to_string()
            } else {
                String::new()
            },
            mode_id,
            stream: visitor
                .stream
                .unwrap_or_else(|| DEFAULT_LOG_STREAM.to_string()),
            message: visitor.message.unwrap_or_default(),
            fields: visitor.fields,
        };

        let mut line = serde_json::to_string(&record).expect("log record should serialize");
        line.push('\n');
        let id = record.mode_id.map(|id| id.to_string());
        if let Err(error) = self.router.write_line_for(id.as_deref(), &line) {
            eprintln!("failed writing structured log record: {error}");
        }
    }

    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = IdExtractVisitor::default();
        attrs.record(&mut visitor);
        if let Some(span) = ctx.span(id)
            && let Some(mode_id) = visitor.mode_id
        {
            span.extensions_mut().insert(CapturedModeId(mode_id));
        }
    }
}

#[derive(Default)]
struct IdExtractVisitor {
    mode_id: Option<String>,
    message: Option<String>,
    stream: Option<String>,
    fields: BTreeMap<String, Value>,
}

#[derive(Debug)]
struct CapturedModeId(String);

fn debug_text(value: &dyn std::fmt::Debug) -> String {
    let value = format!("{value:?}");
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(&value)
        .to_string()
}

fn normalized_mode_id(value: &str) -> Option<String> {
    let value = value.trim_matches('"');
    let value = value
        .strip_prefix("AutonomyModeId(")
        .and_then(|value| value.strip_suffix(')'))
        .unwrap_or(value);
    Uuid::parse_str(value).ok().map(|id| id.to_string())
}

impl tracing::field::Visit for IdExtractVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "id" | "mode_id" => {
                if let Some(id) = normalized_mode_id(value) {
                    self.mode_id = Some(id);
                } else {
                    self.fields
                        .insert(field.name().to_string(), Value::String(value.to_string()));
                }
            }
            "message" => self.message = Some(value.to_string()),
            "stream" => self.stream = Some(value.to_string()),
            name => {
                self.fields
                    .insert(name.to_string(), Value::String(value.to_string()));
            }
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let value = debug_text(value);
        match field.name() {
            "id" | "mode_id" => {
                if let Some(id) = normalized_mode_id(&value) {
                    self.mode_id = Some(id);
                } else {
                    self.fields
                        .insert(field.name().to_string(), Value::String(value));
                }
            }
            "message" => self.message = Some(value),
            "stream" => self.stream = Some(value),
            name => {
                self.fields.insert(name.to_string(), Value::String(value));
            }
        }
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Number::from_f64(value)
                .map(Value::Number)
                .unwrap_or_else(|| Value::String(value.to_string())),
        );
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields.insert(
            field.name().to_string(),
            Value::Number(serde_json::Number::from(value)),
        );
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields.insert(
            field.name().to_string(),
            Value::Number(serde_json::Number::from(value)),
        );
    }

    fn record_i128(&mut self, field: &tracing::field::Field, value: i128) {
        self.fields
            .insert(field.name().to_string(), Value::String(value.to_string()));
    }

    fn record_u128(&mut self, field: &tracing::field::Field, value: u128) {
        self.fields
            .insert(field.name().to_string(), Value::String(value.to_string()));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), Value::Bool(value));
    }

    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.fields
            .insert(field.name().to_string(), Value::String(value.to_string()));
    }
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

fn chrono_like_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_filename_replaces_unsafe_chars() {
        assert_eq!(sanitize_filename("NoImages/1"), "NoImages_1");
        assert_eq!(sanitize_filename("Hive Mast"), "Hive_Mast");
    }

    #[test]
    fn normalizes_debug_mode_ids() {
        let id = "123e4567-e89b-12d3-a456-426614174000";
        assert_eq!(normalized_mode_id(id), Some(id.to_string()));
        assert_eq!(
            normalized_mode_id("AutonomyModeId(123e4567-e89b-12d3-a456-426614174000)"),
            Some(id.to_string())
        );
        assert_eq!(normalized_mode_id("request-123"), None);
    }

    #[test]
    fn timestamps_are_rfc3339() {
        let timestamp = chrono_like_now();
        assert!(timestamp.ends_with('Z'));
        assert!(chrono::DateTime::parse_from_rfc3339(&timestamp).is_ok());
    }
}
