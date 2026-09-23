//! Mode-local host actions. The LLM can select an action ID, never a program or arguments.
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::json;
use tracing::info;

use crate::simulation::PairedSimulation;

#[derive(Debug, Serialize)]
pub(crate) struct ShutdownIntent {
    pub(crate) assessment_id: String,
    pub(crate) anomaly_id: String,
    pub(crate) generation: u64,
    pub(crate) telemetry_version: u64,
    pub(crate) board_version: u64,
    pub(crate) simulation: PairedSimulation,
}

#[async_trait]
pub(crate) trait ShutdownExecutor: Send + Sync {
    async fn shutdown(&self) -> Result<()>;
}

pub(crate) struct LinuxShutdown;

#[async_trait]
impl ShutdownExecutor for LinuxShutdown {
    async fn shutdown(&self) -> Result<()> {
        if !cfg!(target_os = "linux") {
            bail!("host shutdown is supported only on Linux");
        }
        invoke_shutdown(shutdown_command(), Duration::from_secs(5)).await
    }
}

async fn invoke_shutdown(mut command: tokio::process::Command, timeout: Duration) -> Result<()> {
    let mut child = command
        .spawn()
        .context("could not invoke /sbin/shutdown -h now")?;
    let status = tokio::time::timeout(timeout, child.wait())
        .await
        .context("shutdown invocation timed out; host outcome is unknown")??;
    if !status.success() {
        bail!("shutdown invocation failed: {status}");
    }
    Ok(())
}

fn shutdown_command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new("/sbin/shutdown");
    command
        .args(["-h", "now"])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

pub(crate) const SHUTDOWN_JOURNAL: &str = "shutdown-attempt.jsonl";

pub(crate) struct ShutdownController {
    executor: Arc<dyn ShutdownExecutor>,
    attempted: bool,
}

impl Default for ShutdownController {
    fn default() -> Self {
        Self {
            executor: Arc::new(LinuxShutdown),
            attempted: false,
        }
    }
}

impl ShutdownController {
    #[cfg(test)]
    pub(crate) fn with_executor(executor: Arc<dyn ShutdownExecutor>) -> Self {
        Self {
            executor,
            attempted: false,
        }
    }

    /// Called only by the serialized mode handler after its final state checks.
    /// Reserve and sync before invoking: a crash at any later point must not retry
    /// an action whose host outcome may be unknown.
    pub(crate) async fn execute(
        &mut self,
        intent: &ShutdownIntent,
        directory: &Path,
    ) -> Result<()> {
        if self.attempted {
            bail!("shutdown was already attempted by this mode");
        }
        let path = directory.join(SHUTDOWN_JOURNAL);
        let mut journal = OpenOptions::new().write(true).create_new(true).open(&path)
            .with_context(|| format!("cannot reserve shutdown attempt at {} (an existing journal suppresses retries)", path.display()))?;
        self.attempted = true;
        writeln!(
            journal,
            "{}",
            json!({"state": "attempting", "intent": intent})
        )?;
        journal.sync_all()?;
        File::open(directory)?.sync_all()?;
        info!(assessment_id = %intent.assessment_id, anomaly_id = %intent.anomaly_id,
            "power-validated anomaly recovery invoking /sbin/shutdown -h now");
        let result = self.executor.shutdown().await;
        let record = match &result {
            Ok(()) => {
                json!({"state": "accepted", "detail": "shutdown command succeeded; power-off is not acknowledged"})
            }
            Err(error) => json!({"state": "failed_or_unknown", "detail": format!("{error:#}")}),
        };
        writeln!(journal, "{record}")?;
        journal.sync_all()?;
        result
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::simulation::ScenarioRun;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub(crate) fn intent() -> ShutdownIntent {
        let run = ScenarioRun {
            scenario_id: "baseline".into(),
            success: true,
            timed_out: false,
            evidence_revision: 1,
            horizon_days: 0.001,
            metrics: Default::default(),
        };
        ShutdownIntent {
            assessment_id: "assessment-1".into(),
            anomaly_id: "example-hot".into(),
            generation: 1,
            telemetry_version: 1,
            board_version: 1,
            simulation: PairedSimulation {
                thermal_benefit_verified: false,
                baseline: run.clone(),
                recovery: ScenarioRun {
                    scenario_id: "shutdown".into(),
                    ..run
                },
            },
        }
    }

    struct FakeExecutor {
        calls: AtomicUsize,
        fail: bool,
    }

    #[async_trait]
    impl ShutdownExecutor for FakeExecutor {
        async fn shutdown(&self) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                bail!("permission denied");
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn durable_attempt_prevents_reexecution_including_after_restart_and_failure() {
        for fail in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let executor = Arc::new(FakeExecutor {
                calls: AtomicUsize::new(0),
                fail,
            });
            let mut controller = ShutdownController::with_executor(executor.clone());
            assert_eq!(
                controller
                    .execute(&intent(), directory.path())
                    .await
                    .is_err(),
                fail
            );
            assert!(
                controller
                    .execute(&intent(), directory.path())
                    .await
                    .is_err()
            );
            let mut restarted = ShutdownController::with_executor(executor.clone());
            assert!(
                restarted
                    .execute(&intent(), directory.path())
                    .await
                    .is_err()
            );
            assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
            let journal = std::fs::read_to_string(directory.path().join(SHUTDOWN_JOURNAL)).unwrap();
            assert!(journal.contains("attempting"));
            assert!(journal.contains(if fail {
                "failed_or_unknown"
            } else {
                "accepted"
            }));
        }
    }

    #[tokio::test]
    async fn journal_failure_prevents_execution() {
        let directory = tempfile::tempdir().unwrap();
        let executor = Arc::new(FakeExecutor {
            calls: AtomicUsize::new(0),
            fail: false,
        });
        let mut controller = ShutdownController::with_executor(executor.clone());
        assert!(
            controller
                .execute(&intent(), &directory.path().join("missing"))
                .await
                .is_err()
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn linux_command_has_fixed_executable_and_arguments() {
        let command = shutdown_command();
        assert_eq!(command.as_std().get_program(), "/sbin/shutdown");
        assert_eq!(
            command.as_std().get_args().collect::<Vec<_>>(),
            ["-h", "now"]
        );
    }

    #[tokio::test]
    async fn invocation_reports_missing_executable_unsuccessful_exit_and_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let missing = tokio::process::Command::new(directory.path().join("missing-shutdown"));
        assert!(
            invoke_shutdown(missing, Duration::from_secs(1))
                .await
                .unwrap_err()
                .to_string()
                .contains("could not invoke")
        );
        let unsuccessful = tokio::process::Command::new("/bin/false");
        assert!(
            invoke_shutdown(unsuccessful, Duration::from_secs(1))
                .await
                .unwrap_err()
                .to_string()
                .contains("failed")
        );
        let mut slow = tokio::process::Command::new("/bin/sleep");
        slow.arg("1").kill_on_drop(true);
        assert!(
            invoke_shutdown(slow, Duration::from_millis(10))
                .await
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
    }
}
