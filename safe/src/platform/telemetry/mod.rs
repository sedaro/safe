use tokio::sync::mpsc;

use crate::config::Config;
use crate::telemetry_frame::TelemetryFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryIngressKind {
    Disabled,
    Example,
    External,
}

impl TelemetryIngressKind {
    pub fn from_config(name: &str) -> anyhow::Result<Self> {
        match name {
            "disabled" => Ok(Self::Disabled),
            "example" => Ok(Self::Example),
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
        TelemetryIngressKind::Disabled => {}
        TelemetryIngressKind::Example => {
            tokio::spawn(example::example_telemetry_reader(telemetry_tx));
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
                payload: serde_json::json!({"telemetry": {"batt_v": counter, "batt_c": counter}}),
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

#[cfg(test)]
mod tests {
    use super::TelemetryIngressKind;

    #[test]
    fn bash_mock_adapter_is_no_longer_supported() {
        assert!(TelemetryIngressKind::from_config("bash_mock").is_err());
    }

    #[test]
    fn disabled_adapter_is_supported() {
        assert_eq!(
            TelemetryIngressKind::from_config("disabled").unwrap(),
            TelemetryIngressKind::Disabled
        );
    }
}
