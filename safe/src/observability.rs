use anyhow::Result;
use rand_distr::num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::process;
use std::sync::Arc;
use sysinfo::{Pid, System};
use tokio::time;
use tracing::warn;
use crate::c2::{Command, Telemetry};
use crate::config::Config;

pub struct ObservabilitySubsystem {
    sig: String, // Signature for differentiating concurrent log streams
    seq: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub enum Event {
    MetricsCollected {
        uptime: u64,
        memory: f64,
        disk_read: f64,
        disk_write: f64,
        cpu: f32,
    },
    CommandIssued {
        commands: Vec<Command>,
        reason: Option<String>,
    },
    TelemetryReceived(Telemetry),
    // ConfigChanged { before: , after },
}
#[derive(Clone, Serialize, Deserialize, Debug)]
pub enum Location {
    Main,
    Router,
    AutonomyMode(String),
    Config,
    C2,
}

impl ObservabilitySubsystem {
    pub fn new(seq: Option<u64>) -> Self {
        Self {
            sig: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .to_string(),
            seq: Arc::new(std::sync::atomic::AtomicU64::new(seq.unwrap_or(0))),
        }
    }

    pub fn log_event(&self, location: Location, event: Event) {
        let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tracing::info!(sig = self.sig, seq = seq, loc = ?location, event = ?event);
    }

    pub async fn run(&self, config: &Config) -> Result<()> {
        let mut sys = System::new_all();
        let pid = Pid::from(process::id() as usize);
        let mut interval = time::interval(std::time::Duration::from_secs(
            config.observability.metrics.period_seconds,
        ));
        interval.tick().await;
        loop {
            sys.refresh_process(pid);
            let Some(process) = sys.process(pid) else {
                warn!("Failed to get process info for metrics");
                continue;
            };
            self.log_event(
                // TODO: Validate and include entries for each autonomy mode (and all other parallel processes).  This will also not account for anything running outside of the process that is connected in via IPC.  Modes will need their own way or reporting possibly.
                Location::Main,
                Event::MetricsCollected {
                    uptime: process.run_time(),                                   // seconds
                    memory: process.memory().to_f64().unwrap() / 1024.0 / 1024.0, // MB
                    disk_read: process.disk_usage().read_bytes.to_f64().unwrap() / 1024.0 / 1024.0, // MB
                    disk_write: process.disk_usage().written_bytes.to_f64().unwrap()
                        / 1024.0
                        / 1024.0, // MB
                    cpu: process.cpu_usage(), // percentage
                },
            );
            interval.tick().await;
        }
    }
}
