use std::path::Path;
use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use tracing::{Event, Subscriber};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use tracing_subscriber::{
    fmt::{FmtContext, FormatEvent, FormatFields, MakeWriter, format::Writer as FmtWriter},
    registry::LookupSpan,
};
use uuid::Uuid;

use crate::config::Config;

tokio::task_local! {
    pub static CURRENT_TASK_ID: Uuid;
}

/// Keep this alive for non-blocking appenders if you add them later.
pub struct TelemetryGuards;

/// Initialize tracing with:
/// - stdout output
/// - per-id file output: logs/<id>.log when `id` field exists
/// - fallback file: logs/default.log when no `id`
pub fn init_tracing(cfg: &Config) -> Result<TelemetryGuards, Box<dyn std::error::Error>> {
    let filter = EnvFilter::try_new(cfg.tracing.filter.clone())
        .or_else(|_| EnvFilter::try_new(cfg.tracing.level.clone()))?;

    let base_path = PathBuf::from(&cfg.logging.file_path);
    let logs_dir = base_path
        .parent()
        .unwrap_or_else(|| Path::new("logs"))
        .to_path_buf();
    println!("Logging directory: {logs_dir:?}");
    std::fs::create_dir_all(&logs_dir)?;

    let router = PerIdMakeWriter::new(logs_dir, "default.log");

    let per_id_layer = PerIdFileLayer::new(router.clone(), cfg.tracing.with_target);

    let stdout_layer = fmt::layer()
        .with_ansi(true)
        .with_target(cfg.tracing.with_target)
        .with_writer(std::io::stdout);

    tracing_subscriber::registry()
        .with(filter)
        .with(stdout_layer)
        .with(per_id_layer)
        .init();

    Ok(TelemetryGuards)
}

/// Routes events to `<id>.log` if an `id` field exists, else `default.log`.
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

/// NOTE: this is used by our formatter directly; `make_writer` default path is fallback.
impl PerIdMakeWriter {
    fn write_line_for(&self, id: Option<&str>, line: &str) -> io::Result<()> {
        let mut w = self.writer_for_id(id);
        w.write_all(line.as_bytes())?;
        w.flush()
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

        let f = state
            .files
            .get_mut(&file_name)
            .expect("inserted file missing");
        f.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self.state.lock().expect("poisoned mutex");

        let file_name = if self.key == "default" {
            state.fallback_file.clone()
        } else {
            format!("{}.log", self.key)
        };

        if let Some(f) = state.files.get_mut(&file_name) {
            f.flush()
        } else {
            Ok(())
        }
    }
}

/// Custom formatter that extracts `id` from event fields and writes to per-id file.
#[allow(unused)]
struct IdAwareFormatter;

impl<S, N> FormatEvent<S, N> for IdAwareFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: FmtWriter<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        // Render a normal formatted line first.
        let mut line = String::new();
        {
            let mut tmp_writer = FmtWriter::new(&mut line);
            let meta = event.metadata();
            let _ = write!(
                tmp_writer,
                "{} {} {}: ",
                chrono_like_now(),
                meta.level(),
                meta.target()
            );

            // Include span context if present.
            if let Some(scope) = ctx.event_scope() {
                for span in scope.from_root() {
                    let _ = write!(tmp_writer, "[{}] ", span.name());
                }
            }

            // Append fields
            let mut visitor = IdExtractVisitor::default();
            event.record(&mut visitor);
            if let Some(msg) = visitor.message {
                let _ = write!(tmp_writer, "{}", msg);
            }
            if !visitor.rest.is_empty() {
                let _ = write!(tmp_writer, " {}", visitor.rest.join(" "));
            }
            let _ = writeln!(tmp_writer);
        }

        writer.write_str(&line)?;

        let mut visitor = IdExtractVisitor::default();
        event.record(&mut visitor);

        Ok(())
    }
}

/// A dedicated layer that writes each event to per-id files.
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
        if visitor.id.is_none() {
            if let Some(scope) = ctx.event_scope(event) {
                for span in scope.from_root() {
                    if let Some(id) = span.extensions().get::<CapturedId>() {
                        visitor.id = Some(id.0.clone());
                        break;
                    }
                }
            }
        }

        let mut line = String::new();
        if self.with_target {
            line.push_str(&format!(
                "{} {} {} ",
                chrono_like_now(),
                meta.level(),
                meta.target()
            ));
        } else {
            line.push_str(&format!("{} {} ", chrono_like_now(), meta.level()));
        }

        if let Some(msg) = visitor.message {
            line.push_str(&msg);
        }
        if !visitor.rest.is_empty() {
            line.push(' ');
            line.push_str(&visitor.rest.join(" "));
        }
        line.push('\n');

        let _ = self.router.write_line_for(visitor.id.as_deref(), &line);
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
            && let Some(captured_id) = visitor.id
        {
            span.extensions_mut().insert(CapturedId(captured_id));
        }
    }
}

#[derive(Default)]
struct IdExtractVisitor {
    id: Option<String>,
    message: Option<String>,
    rest: Vec<String>,
}

#[derive(Debug)]
struct CapturedId(String);

impl tracing::field::Visit for IdExtractVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "id" => self.id = Some(value.to_string()),
            "message" => self.message = Some(value.to_string()),
            name => self.rest.push(format!(r#"{name}="{value}""#)),
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let v = format!("{value:?}");
        match field.name() {
            "id" => self.id = Some(v.trim_matches('"').to_string()),
            "message" => self.message = Some(v),
            name => self.rest.push(format!(r#"{name}={v}"#)),
        }
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
    format!("{:?}", std::time::SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_filename_replaces_unsafe_chars() {
        assert_eq!(sanitize_filename("NoImages/1"), "NoImages_1");
        assert_eq!(sanitize_filename("Hive Mast"), "Hive_Mast");
    }
}
