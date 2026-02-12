use anyhow::Result;
use async_trait::async_trait;
use simvm::sv::pretty::Pretty;
use crate::c2::{Command, Telemetry, TimedCommand};
use crate::definitions::Activation;
use crate::router::AutonomyMode;
use crate::utils::utc_mjd_to_gps;
use serde::Serialize;
use simvm::sv::data::Data;
use core::panic;
use std::vec;

use simvm::sv::combine::TRD;
use crate::c2::{AutonomyModeMessage, RouterMessage};
use crate::transports::Stream;
use crate::simulation::{FileTargetReader, SedaroSimulator};

#[derive(Debug, Serialize)]
pub struct ContactAnalysis {
    name: String,
    priority: u8,
    activation: Activation,
    simulator: SedaroSimulator,
}
#[async_trait]
impl AutonomyMode<Telemetry, TimedCommand> for ContactAnalysis {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn priority(&self) -> u8 {
        self.priority
    }
    fn activation(&self) -> Activation {
        self.activation.clone()
    }
    async fn run(&mut self, mut stream: Box<dyn Stream<AutonomyModeMessage<Telemetry>, RouterMessage<TimedCommand>>>) -> Result<()> {
      let mut active = false;
      let mut nonce: Option<u64> = None;
      loop {
        if let Ok(message) = stream.read().await {
          match message {
            AutonomyModeMessage::Active { nonce: new_nonce } => {
              active = true;
              nonce = Some(new_nonce);
              println!("ContactAnalysis activated");

              let agent_id = "PTnYWzsc2Nhywc8WVS4blm";
              let results_path = tempfile::Builder::new()
                .prefix("simulation_results_")
                .tempdir()?
                .into_path();
                            
              let result = self.simulator.run(30.0*60.0/86_400.0, &results_path, None).await;
              match &result {
                Ok(output) => {
                  match output.status.success() {
                    true => {
                      println!("Simulation completed successfully");
                    },
                    false => {
                      panic!("Simulation failed with non-zero exit code: {:?}", String::from_utf8_lossy(&output.stderr)); // FIXME: warn instead?
                    },
                  }
                },
                Err(e) => {
                  panic!("Simulation failed: {:?}", e); // FIXME: warn instead?
                },
              }

              let mut reader = FileTargetReader::try_from_path(
                &results_path.join(format!("{agent_id}.gnc.jsonl"))
              ).expect("Failed to create FileTargetReader");
              let frames = reader.read_frames().unwrap();
              let mut optimal_contact_angle = f64::MAX;
              let mut optimal_timestamp_mjd = 0.0;
              for frame in &frames {
                let angle = min_contact_angle_to_iridium(frame);
                if angle < optimal_contact_angle {
                  optimal_contact_angle = angle;
                  optimal_timestamp_mjd = frame.get_by_field("time").unwrap().data.as_f64().unwrap();
                }
              }

              println!("Optimal contact angle: {} degrees at MJD {}", optimal_contact_angle, optimal_timestamp_mjd);

              stream
                .write(RouterMessage::Command { 
                  data: TimedCommand::Scheduled {
                    cmd: Command::IridiumTransmitMsg("Hello from SAFE!".to_string()),
                    gps_time: utc_mjd_to_gps(optimal_timestamp_mjd),
                  },
                  nonce: new_nonce,
                })
                .await?;
            },
            AutonomyModeMessage::Inactive => {
              active = false;
              println!("ContactAnalysis deactivated");
            },
            AutonomyModeMessage::Telemetry(_telemetry) => {
              // println!("ContactAnalysis received telemetry {:?}", _telemetry);
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

fn min_contact_angle_to_iridium(frame: &TRD) -> f64 {
  let iridium_ids = vec![
    "PVV4vsG6Y6cvnTwGcHs5Dh",
    "PVV4vsG8XxxVFD2ZzXqxH4",
    "PVV4vsGBTBwS5PdWPpMYMn",
    "PVV4vsGDTZd9fr2jncSKc6",
    "PVV4vsGGVKvvfLxLXqNkfW",
    "PVV4vsGJNsVqdzmmDs66Q9",
    "PVV4vsGLPSQFC4yQs3pMgt",
    "PVV4vsGNMNKtdhfhj3hfmz",
    "PVV4vsGQPHjyqNJxtddZMS",
    "PVV4vsGSLyhd7rhh9xcCzh",
    "PVV4vsGVMw37DssYlhkQJG",
    "PVV4vsGXM6N8VwmTzQGkDC",
    "PVV4vsGZJYT3JwM2kKly7v",
    "PVV4vsGcJXK57v3SfWxLRK",
    "PVV4vsGfJCstRztdwrp2CD",
    "PVV4vsGhJdgV2Cz6QXG4Dm",
    "PVV4vsGkHt64zSg8g6ZgWf",
    "PVV4vsGmFxf6lhJHpMZS9c",
    "PVV4vsGpFNxyDpWyZRl3K8",
    "PVV4vsGrBzhyjr46pztn3X",
    "PVV4vsGt9HgyCXtFdSNnNk",
    "PVV4vsGwBRKJMvFfy58df5",
    "PVV4vsGy5GwzldWpw2jJn9",
    "PVV4vsH29FQ2ycZ5mgHSQt",
    "PVV4vsH47YZP99YCskX8RH",
    "PVV4vsH67QJ4XnmHZDjtqV",
    "PVV4vsH7z3L6QSxfn5ch9n",
    "PVV4vsH9zR9qFHrG6HSgzT",
    "PVV4vsHD3n53yMW2yCTrBD",
    "PVV4vsHG2zWMzkJqpQHP63",
    "PVV4vsHHywH6Zfsqz4VtxF",
    "PVV4vsHKzRY6bKJJBvShxm",
    "PVV4vsHMwRXQ3X2vywCWJL",
    "PVV4vsHPwhQgLlzrvl4BHx",
    "PVV4vsHRv6DlWDSpwPYJzX",
    "PVV4vsHTpWFqhth87LRPHg",
    "PVV4vsHWnKtp86L8R7YfbJ",
    "PVV4vsHYpczTBHxq6JcxFm",
    "PVV4vsHbnvF6ndgVKLy4nR",
    "PVV4vsHdmGSHzcM6FyVkqt",
    "PVV4vsHgjBkKz5QlNXBrM2",
    "PVV4vsHjlHCDtg3LTGdgHB",
    "PVV4vsHlmd9ly6fr5pQJyY",
    "PVV4vsHnfVNHWQSRWZZTjr",
    "PVV4vsHqgHzt2zX6PQzvTX",
    "PVV4vsHsgGq3DDBSxb8mJM",
    "PVV4vsHvhGthzWtWx4JmwR",
    "PVV4vsHxd8SwlLbhNmZb6F",
    "PVV4vsHzf7FflGjCr9XLyX",
    "PVV4vsJ3YpvsP6XpkLq8FL",
    "PVV4vsJ5XvmGxbmkNq2bRt",
    "PVV4vsJ7bsqq7tCtKrWVFh",
    "PVV4vsJ9Y2gw4TYXF2fFmr",
    "PVV4vsJCXpDySblRs2BmbP",
    "PVV4vsJFTkRGr2bdrNG6x3",
    "PVV4vsJHTgqklZ2kqw5WGL",
    "PVV4vsJKTTN9v5bwykpXyT",
    "PVV4vsJMSKqtCngJZCpgrn",
    "PVV4vsJPPMjFYC5KYhkv82",
    "PVV4vsJRRW4KD25zj2nctf",
    "PVV4vsJTN76KZqLj9RRwkN",
    "PVV4vsJWNyM2zwQvB73CGP",
    "PVV4vsJYLBqfTM27JvjFht",
    "PVV4vsJbLxzsTmr6QSgYgz",
    "PVV4vsJdHFCyP7y3SKcZMv",
    "PVV4vsJgFDysgtPB2pVstg",
    "PVV4vsJjFTBkKjRxX4GwyT",
    "PVV4vsJlDmXbzYldDtsrmQ",
    "PVV4vsJnCMYTd9tLXWs5hb",
    "PVV4vsJqF3T6ZzNVLfxLJk",
    "PVV4vsJsFsgP7w7TB845pC",
    "PVV4vsJv9Cn4C2P4Gvr93S",
    "PVV4vsJx7yvkJxZVSvdVMK",
    "PVV4vsJzBsDB75VtTxQcV4",
    "PVV4vsK39FMGrBbsD4lgSb",
    "PVV4vsK54JSwpsLHjrbhg4",
    "PVV4vsK72DvJ3X3lwkZSwP",
    "PVV4vsK96jWNQGzLtbrrWY",
    "PVV4vsKBymckGQ7lMcFhct",
    "PVV4vsKF596L9cmPDrRNkv",
  ];

  let mut min_angle = f64::MAX;
  for id in iridium_ids {
    let curr_angle = frame.get_by_field(format!("{}.contact_angle", id).as_str()).unwrap().data.as_f64().unwrap();
    if min_angle > curr_angle {
      min_angle = curr_angle;
    }
  }
  min_angle
}
