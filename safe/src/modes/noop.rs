use anyhow::Result;
use async_trait::async_trait;
use crate::c2::{Command, Telemetry};
use crate::definitions::Activation;
use crate::router::AutonomyMode;
use serde::Serialize;

use crate::c2::{AutonomyModeMessage, RouterMessage};
use crate::transports::Stream;

#[derive(Debug, Serialize)]
pub struct Noop {
    name: String,
    priority: u8,
    activation: Activation,
}
#[async_trait]
impl AutonomyMode<Telemetry, Command> for Noop {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn priority(&self) -> u8 {
        self.priority
    }
    fn activation(&self) -> Activation {
        self.activation.clone()
    }
    async fn run(&mut self, stream: Box<dyn Stream<AutonomyModeMessage<Telemetry>, RouterMessage<Command>>>) -> Result<()> {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
        }
    }
}
impl Noop {
    pub fn new(name: &str, priority: u8, activation: Activation) -> Self {
        Self {
            name: name.to_string(),
            priority,
            activation,
        }
    }
}