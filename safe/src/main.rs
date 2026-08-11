#![allow(unused)]

mod config;
mod config_paths;
mod definitions;
mod flight;
mod platform;
mod protocol;
mod router;
mod runtime;
mod safetea;
mod sandbox;
mod telemetry_frame;
#[cfg(test)]
mod tests;
mod transports;
mod utils;

use std::path::PathBuf;

pub use protocol::{
    AutonomyModeId, AutonomyModeInput, AutonomyModeOutput, BoardCmdId, BoardEvent, BoardState,
    Command, CommandEnvelope, ModeToSafe, SafeToMode, TimedCommand,
};
pub use runtime::ProcessResourceSnapshot;
pub use runtime::{
    AutonomyModeMeta, ExternalCommand, HostCommandDispatchRecord, HostCommandRequest,
    HostCommandStatus, HostCommandStatusState, ModeResourceSnapshot, SafectlIngress,
};
use tokio::fs;

use crate::config::Config;
pub use crate::flight::AutonomyModeActivation;
use crate::safetea::SafeTEA;
use crate::sandbox::logging;

struct PidFileGuard {
    path: PathBuf,
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug, Clone, Default)]
struct RuntimePaths {
    base: PathBuf,
    state: PathBuf,
    flight: PathBuf,
    events: PathBuf,
    outputs: PathBuf,
    pid: PathBuf,
    safectl_sock: PathBuf,
    summary: PathBuf,
    status: PathBuf,
}

impl RuntimePaths {
    fn new(cfg: &Config) -> Self {
        let base_writable_path = &cfg.base_paths.base_writable_directory;
        let base_writable_path = PathBuf::from(base_writable_path);
        let state_dir = base_writable_path.join("state");
        let flight_path = state_dir.join("flight.json");
        let events_path = state_dir.join("events.jsonl");
        let outputs_path = state_dir.join("outputs.jsonl");
        let pid_path = state_dir.join("safe.pid");
        let safectl_sock_path = state_dir.join("safectl.sock");
        let summary_path = base_writable_path.join("out").join("summary.json");
        let status_path = state_dir.join("status.json");

        Self {
            base: base_writable_path,
            state: state_dir,
            flight: flight_path,
            events: events_path,
            outputs: outputs_path,
            pid: pid_path,
            safectl_sock: safectl_sock_path,
            summary: summary_path,
            status: status_path,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load()?;
    cfg.validate()
        .map_err(|e| format!("config error: {e}"))
        .expect("config invalid");

    let _log_guard = logging::init_tracing(&cfg).expect("logging init");

    let runtime_paths = RuntimePaths::new(&cfg);
    fs::create_dir_all(&runtime_paths.state).await?;
    fs::write(&runtime_paths.pid, format!("{}\n", std::process::id())).await?;
    let _pid_guard = PidFileGuard {
        path: runtime_paths.pid.clone(),
    };

    let mut safetea = SafeTEA::new(cfg, runtime_paths).await;

    // Main entrypoint for SAFE
    safetea.run().await;

    Ok(())
}
