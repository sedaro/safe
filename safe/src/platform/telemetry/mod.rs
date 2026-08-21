use tokio::sync::mpsc;

use crate::config::Config;
use crate::telemetry_frame::TelemetryFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryIngressKind {
    Example,
    BashMock,
    External,
}

impl TelemetryIngressKind {
    pub fn from_config(name: &str) -> anyhow::Result<Self> {
        match name {
            "example" => Ok(Self::Example),
            "bash_mock" => Ok(Self::BashMock),
            "external" => Ok(Self::External),
            _ => anyhow::bail!("unsupported telemetry ingress adapter: {name}"),
        }
    }
}

pub fn spawn_telemetry_ingress(
    cfg: &Config,
    telemetry_tx: mpsc::Sender<TelemetryFrame>,
) -> anyhow::Result<()> {
    let telemetry_kind = TelemetryIngressKind::from_config(&cfg.platform.telemetry_adapter)?;

    match telemetry_kind {
        TelemetryIngressKind::Example => {
            tokio::spawn(example::example_telemetry_reader(telemetry_tx));
        }
        TelemetryIngressKind::BashMock => {
            #[cfg(feature = "platform-bash-mock")]
            {
                tokio::spawn(bash_mock::bash_mock_telemetry_reader(
                    cfg.platform
                        .bash_mock_telemetry_command
                        .clone()
                        .unwrap_or_else(|| "scripts/mock_telemetry.sh".to_string()),
                    telemetry_tx,
                ));
            }
            #[cfg(not(feature = "platform-bash-mock"))]
            {
                anyhow::bail!(
                    "telemetry adapter bash_mock selected, but feature platform-bash-mock is disabled"
                );
            }
        }
        TelemetryIngressKind::External => {
            let command = cfg
                .platform
                .external_telemetry_command
                .clone()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "telemetry adapter external selected, but platform.external_telemetry_command is not configured"
                    )
                })?;
            tokio::spawn(external::external_telemetry_reader(command, telemetry_tx));
        }
    }

    Ok(())
}

mod external {
    use tokio::io::AsyncBufReadExt;
    use tokio::io::BufReader;
    use tokio::process::Command;
    use tokio::sync::mpsc;
    use tracing::{error, info};

    use crate::telemetry_frame::TelemetryFrame;

    pub async fn external_telemetry_reader(
        command: String,
        tx: mpsc::Sender<TelemetryFrame>,
    ) -> anyhow::Result<()> {
        info!(%command, "platform telemetry adapter `external` started");

        let mut child = Command::new("bash")
            .arg("-lc")
            .arg(command)
            .stdout(std::process::Stdio::piped())
            .spawn()?;

        let Some(stdout) = child.stdout.take() else {
            anyhow::bail!("external telemetry process has no stdout");
        };

        let mut lines = BufReader::new(stdout).lines();

        while let Some(line) = lines.next_line().await? {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let value = match serde_json::from_str::<serde_json::Value>(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    error!("invalid external telemetry json value: {e}; line={trimmed}");
                    continue;
                }
            };

            let frame = TelemetryFrame {
                source: value
                    .get("source")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string),
                ts_mono: value.get("ts_mono").and_then(|v| v.as_u64()).unwrap_or(0),
                payload: value
                    .get("payload")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            };

            if tx.send(frame).await.is_err() {
                break;
            }
        }

        Ok(())
    }
}

mod example {
    use tokio::sync::mpsc;
    use tokio::time::{Duration, sleep};
    use tracing::info;

    use crate::telemetry_frame::TelemetryFrame;

    pub async fn example_telemetry_reader(tx: mpsc::Sender<TelemetryFrame>) -> anyhow::Result<()> {
        info!("platform telemetry adapter `example` started");

        let mut counter: u32 = 0;
        loop {
            let t = TelemetryFrame {
                source: Some("example".to_string()),
                ts_mono: counter as u64,
                payload: serde_json::json!({"telemetry": {"batt_v": counter}}),
            };

            if tx.send(t).await.is_err() {
                break;
            }

            counter = counter.wrapping_add(1);
            sleep(Duration::from_secs(1)).await;
        }

        Ok(())
    }
}

#[cfg(feature = "platform-bash-mock")]
mod bash_mock {
    use tokio::io::AsyncBufReadExt;
    use tokio::io::BufReader;
    use tokio::process::Command;
    use tokio::sync::mpsc;
    use tracing::{error, info};

    use crate::telemetry_frame::TelemetryFrame;

    pub async fn bash_mock_telemetry_reader(
        command: String,
        tx: mpsc::Sender<TelemetryFrame>,
    ) -> anyhow::Result<()> {
        info!(%command, "platform telemetry adapter `bash_mock` started");

        let mut child = Command::new("bash")
            .arg("-lc")
            .arg(command)
            .stdout(std::process::Stdio::piped())
            .spawn()?;

        let Some(stdout) = child.stdout.take() else {
            anyhow::bail!("bash mock telemetry process has no stdout");
        };

        let mut lines = BufReader::new(stdout).lines();
        let mut seq: u64 = 0;

        while let Some(line) = lines.next_line().await? {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let payload = match serde_json::from_str::<serde_json::Value>(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    error!("invalid bash mock telemetry json: {e}; line={trimmed}");
                    continue;
                }
            };

            let ts_mono = payload
                .get("ts_mono")
                .and_then(|v| v.as_u64())
                .unwrap_or(seq);

            if tx
                .send(TelemetryFrame {
                    source: Some("bash_mock".to_string()),
                    ts_mono,
                    payload,
                })
                .await
                .is_err()
            {
                break;
            }

            seq = seq.wrapping_add(1);
        }

        Ok(())
    }
}
