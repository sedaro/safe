use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::observability as obs;
use crate::transports::C2Transport;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Telemetry {
    pub timestamp: u64,
    pub proximity_m: i32,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Command {
    pub commanded_attitude: Vec<u8>,
    pub thrust: u8,
}

// TODO: Revisit
async fn c2_interface_task(
    mut transport: Box<dyn C2Transport>,
    telem_tx: mpsc::Sender<Telemetry>,
    mut command_rx: mpsc::Receiver<Command>,
    observability: Arc<obs::ObservabilitySubsystem>,
) -> Result<()> {
    info!("C2 interface started");

    loop {
        tokio::select! {
            result = transport.recv_telemetry() => {
                match result {
                    Ok(telemetry) => {
                        observability.log_event(
                            obs::Location::C2,
                            obs::Event::TelemetryReceived(telemetry.clone()),
                        );
                        telem_tx.send(telemetry).await?;
                    }
                    Err(e) => {
                        warn!("C2 recv error: {}", e);
                        break;
                    }
                }
            }
            Some(cmd) = command_rx.recv() => {
                transport.send_command(cmd).await?;
            }
        }
    }

    Ok(())
}