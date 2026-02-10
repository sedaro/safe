use anyhow::Result;
use async_trait::async_trait;
use base64::prelude::BASE64_STANDARD;
use crate::c2::{Command, Telemetry};
use crate::definitions::Activation;
use crate::router::AutonomyMode;
use serde::Serialize;
use simvm::sv::data::{Data, FloatValue};
use simvm::sv::ser_de::{dyn_de, dyn_ser};
use std::sync::Arc;
use std::vec;
use tokio::sync::Mutex;
use tracing::{info, warn};
use tokio::sync::Semaphore;
use ordered_float::OrderedFloat;
use tracing::Instrument;
use base64::Engine;

use simvm::sv::{combine::TR, data::Datum, parse::Parse};
use crate::c2::{AutonomyModeMessage, RouterMessage};
use crate::transports::Stream;
use crate::simulation::{self, SedaroSimulator};
use crate::kits::stats::{GuassianSet, NormalDistribution};
use crate::kits::stats::StatisticalDistribution;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Debug, Serialize)]
pub struct ContactAnalysis {
    name: String,
    priority: u8,
    activation: Activation,
    simulator: SedaroSimulator,
}
#[async_trait]
impl AutonomyMode<Telemetry, Command> for ContactAnalysis {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn priority(&self) -> u8 {
        self.priority
    }
    fn activation(&self) -> Activation {
        self.activation.clone()
    }
    async fn run(&mut self, mut stream: Box<dyn Stream<AutonomyModeMessage<Telemetry>, RouterMessage<Command>>>) -> Result<()> {
      let mut active = false;
      let mut nonce: Option<u64> = None;
      loop {
        if let Ok(message) = stream.read().await {
          match message {
            AutonomyModeMessage::Active { nonce: new_nonce } => {
              active = true;
              nonce = Some(new_nonce);
              println!("ContactAnalysis activated");
            },
            AutonomyModeMessage::Inactive => {
              active = false;
              println!("ContactAnalysis deactivated");
            },
            AutonomyModeMessage::Telemetry(_telemetry) => {
              println!("ContactAnalysis received telemetry {:?}", _telemetry);
              // Ignore telemetry for now
            },
          }
        }
      }
    }
}

impl ContactAnalysis {
  pub fn new(name: &str, priority: u8, activation: Activation, simulator: SedaroSimulator) -> Self {
      Self {
          name: name.to_string(),
          priority,
          activation,
          simulator,
      }
  }
}
