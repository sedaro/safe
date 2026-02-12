use anyhow::Result;
use async_trait::async_trait;
use crate::c2::{Command, Telemetry, TimedCommand};
use crate::definitions::Activation;
use crate::router::AutonomyMode;
use serde::Serialize;
use simvm::sv::data::Data;
use std::sync::Arc;
use std::vec;
use tokio::sync::Mutex;
use tracing::{info, warn};
use tokio::sync::Semaphore;
use tracing::Instrument;

use crate::c2::{AutonomyModeMessage, RouterMessage};
use crate::transports::Stream;
use crate::simulation::{FileTargetReader, SedaroSimulator};
use crate::kits::stats::{GuassianSet, NormalDistribution};
use crate::kits::stats::StatisticalDistribution;

#[derive(Debug, Serialize)]
pub struct AttitudeControlAnomalyRecovery {
    name: String,
    priority: u8,
    activation: Activation,
    N: usize,
    concurrency: usize,
    simulator: SedaroSimulator,
}
#[async_trait]
impl AutonomyMode<Telemetry, TimedCommand> for AttitudeControlAnomalyRecovery {
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
              let pid_controller_gains = (5e-5, 0.0, 2.5e-4, 0.01);
              if let Ok(vetted_gains) = self.uq_pid_gains(pid_controller_gains).await {
                stream
                    .write(RouterMessage::Command { 
                      data: TimedCommand::Now(Command::SetPidControllerGains(vetted_gains.0, vetted_gains.1, vetted_gains.2, vetted_gains.3)),
                      nonce: new_nonce,
                    })
                    .await?;
              }
            },
            AutonomyModeMessage::Inactive => {
              active = false;
            },
            AutonomyModeMessage::Telemetry(_telemetry) => {
              // Ignore telemetry for now
            },
          }
        }
      }
    }
}

impl AttitudeControlAnomalyRecovery {
  pub fn new(name: &str, priority: u8, activation: Activation, N: usize, concurrency: usize, simulator: SedaroSimulator) -> Self {
      Self {
          name: name.to_string(),
          priority,
          activation,
          N,
          concurrency,
          simulator,
      }
  }

  async fn uq_pid_gains(&self, pid_controller_gains: (f64, f64, f64, f64)) -> Result<(f64, f64, f64, f64)> {
      info!("Average pointing error is too high.  Re-tuning controller.");
      info!("New PID gains selected: {}, {}, {}, {}", pid_controller_gains.0, pid_controller_gains.1, pid_controller_gains.2, pid_controller_gains.3);

      let semaphore = Arc::new(Semaphore::new(self.concurrency));
      let mut handles = Vec::new();
      let start_time = std::time::Instant::now();
      let success_count = Arc::new(Mutex::new(0.0));
      let fail_count = Arc::new(Mutex::new(0.0));

      let mut inertia_0_0 = 0.005;
      let mut inertia_1_1 = 0.005;
      let mut inertia_2_2 = 0.005;
      let mut x_wheel_inertia = 0.000005;
      let mut y_wheel_inertia = 0.000005;
      let mut z_wheel_inertia = 0.000005;

      let mut inertia_mat_0_0_dist = NormalDistribution::new(inertia_0_0, inertia_0_0 * ((5.0 / 3.0) / 100.0)).seed(10);
      let mut inertia_mat_1_1_dist = NormalDistribution::new(inertia_1_1, inertia_1_1 * ((5.0 / 3.0) / 100.0)).seed(11);
      let mut inertia_mat_2_2_dist = NormalDistribution::new(inertia_2_2, inertia_2_2 * ((5.0 / 3.0) / 100.0)).seed(12);
      let mut x_wheel_inertia_dist = NormalDistribution::new(x_wheel_inertia, x_wheel_inertia * ((5.0 / 3.0) / 100.0)).seed(13);
      let mut y_wheel_inertia_dist = NormalDistribution::new(y_wheel_inertia, y_wheel_inertia * ((5.0 / 3.0) / 100.0)).seed(14);
      let mut z_wheel_inertia_dist = NormalDistribution::new(z_wheel_inertia, z_wheel_inertia * ((5.0 / 3.0) / 100.0)).seed(15);
      let max_speed_observations = Arc::new(Mutex::new(GuassianSet::new()));
      let max_pointing_error_observations = Arc::new(Mutex::new(GuassianSet::new()));

      for i in 0..self.N {
        let permit = semaphore.clone().acquire_owned().await?;
        let simulator = self.simulator.clone();
        
        inertia_0_0 = inertia_mat_0_0_dist.sample();
        inertia_1_1 = inertia_mat_1_1_dist.sample();
        inertia_2_2 = inertia_mat_2_2_dist.sample();
        x_wheel_inertia = x_wheel_inertia_dist.sample();
        y_wheel_inertia = y_wheel_inertia_dist.sample();
        z_wheel_inertia = z_wheel_inertia_dist.sample();
        
        let success_count_clone = success_count.clone();
        let fail_count_clone = fail_count.clone();
        let max_speed_observations_clone = max_speed_observations.clone();
        let max_pointing_error_observations_clone = max_pointing_error_observations.clone();
        let handle = tokio::spawn(async move { // TODO: Try avoiding the spawn?

          const AGENT_ID: &str = "PTnYWzsc2Nhywc8WVS4blm";
          let results_path = tempfile::Builder::new()
            .prefix("simulation_results_")
            .tempdir()?
            .into_path();

          let mut patches = vec![];
          patches.push((
            format!("({AGENT_ID}: (gnc: (\"root!.pid_config\": (float, float, float, float),),),)"), 
            format!("(((({:.15}, {:.15}, {:.15}, {:.15}),),),)", pid_controller_gains.0, pid_controller_gains.1, pid_controller_gains.2, pid_controller_gains.3)
          ));
          patches.push((
            format!("({AGENT_ID}: (gnc: (\"PTnxx2jZ8vzTJlRwjxyctn.inertia\": float, \"PTnxxBv6drdQlzqJlVvvbC.inertia\": float, \"PTnxxMb7DrlvQKWQsDST4r.inertia\": float),),)"), 
            format!("((({:.15}, {:.15}, {:.15}),),)", x_wheel_inertia, y_wheel_inertia, z_wheel_inertia)
          ));
          patches.push((
            format!("({AGENT_ID}: (gnc: (\"root!.inertia\": {{((float, float, float), (float, float, float), (float, float, float)) | #}},),),)"),
            format!("((((({:.15}, 0, 0), (0, {:.15}, 0), (0, 0, {:.15}))),),)", inertia_0_0, inertia_1_1, inertia_2_2)
          ));

          // Clear results
          if results_path.exists() {
            tokio::fs::remove_dir_all(&results_path).await.ok();
          }
          let result = simulator.run(0.5, &results_path, Some(&patches)).await;
          drop(permit); // Release the permit when done
          match &result {
            Ok(output) => {
              match output.status.success() {
                true => {
                  *success_count_clone.lock().await += 1.0;
                },
                false => {
                  warn!("Simulation {} failed with non-zero exit code: {:?}", i, String::from_utf8_lossy(&output.stderr));
                  *fail_count_clone.lock().await += 1.0;
                  return result;
                },
              }
            },
            Err(e) => {
              warn!("Simulation failed: {:?}", e);
              *fail_count_clone.lock().await += 1.0;
              return result;
            },
          }

          // TODO: Handle there being zero frames

          let mut reader = FileTargetReader::try_from_path(
            &results_path.join("PTnYWzsc2Nhywc8WVS4blm.gnc.jsonl")
          )?;
          let frames = reader.read_frames()?;

          let mut max_speed: f64 = 0.0;
          let mut max_pointing_error: f64 = 0.0;
          let mut pointing_errors = vec![];
          let mut i = 0;

          for frame in &frames {
            let speed = frame.get(1).unwrap().data.as_f64().unwrap();
            if max_speed < speed.abs() { max_speed = speed.abs() }
            let speed = frame.get(4).unwrap().data.as_f64().unwrap();
            if max_speed < speed.abs() { max_speed = speed.abs() }
            let speed = frame.get(7).unwrap().data.as_f64().unwrap();
            if max_speed < speed.abs() { max_speed = speed.abs() }
            if i > frames.len()/2 {
              match frame.get_by_field("root.pointing_error").unwrap().data.as_variant().unwrap() { // TODO: Better way?
                (1, datum) => {
                  let pointing_error = datum.as_f64().unwrap();
                  pointing_errors.push(pointing_error);
                  if max_pointing_error < pointing_error.abs() { max_pointing_error = pointing_error.abs() }
                },
                _ => {
                  panic!("Unexpected data type for pointing error");
                }
              }
            }
            // println!("{}", frame.pretty());
            i += 1;
          }        
          let average_error = pointing_errors.iter().sum::<f64>() / (pointing_errors.len() as f64);
          // println!("{} {} {}", frames.len(), max_speed, average_error);
          println!("{} {} {}", frames.len(), max_speed, max_pointing_error);
          max_speed_observations_clone.lock().await.add(max_speed);
          max_pointing_error_observations_clone.lock().await.add(max_pointing_error);
          result
        }.in_current_span());
        
        handles.push(handle);
        
        // If at 5% increments
        if (self.N / 20) > 0 && (i + 1) % (self.N / 20) == 0 {
          let s_count = success_count.clone();
          let s_count = *s_count.lock().await;
          let f_count = fail_count.clone();
          let f_count = *f_count.lock().await;
          info!(
            "Simulation Rate: {} per second ({} successful, {} failed, {} active)", 
            (s_count + f_count)/start_time.elapsed().as_secs_f64(),
            s_count,
            f_count,
            handles.len() - (s_count as usize) - (f_count as usize),
          );
          info!("Interim max wheel speed analysis results: {}", max_speed_observations.lock().await);
          info!("Interim average pointing error analysis results: {}", max_pointing_error_observations.lock().await);
        }
      }

      // Wait for all simulations to complete
      for handle in handles {
        if let Err(e) = handle.await {
          warn!("Simulation task join error: {:?}", e);
        }
      }

      // Decide how to proceed
      let max_speed_observations_locked = max_speed_observations.lock().await;
      let max_pointing_error_observations_locked = max_pointing_error_observations.lock().await;
      info!("Final max wheel speed analysis results: {}", max_speed_observations_locked);
      info!("Final average pointing error analysis results: {}", max_pointing_error_observations_locked);
      info!("Analysis duration: {} ms", start_time.elapsed().as_millis());
      if let Some(wstd_err) = max_speed_observations_locked.std_dev() {
        if let Some(wmean) = max_speed_observations_locked.mean() {
          if let Some(pstd_err) = max_pointing_error_observations_locked.std_dev() {
            if let Some(pmean) = max_pointing_error_observations_locked.mean() {
              let max_wheel_speed = wmean + 3.0 * wstd_err;
              let max_pointing_error = pmean + 3.0 * pstd_err;
              if max_pointing_error < 5.0 && max_wheel_speed < 500.0 {
                info!("Analysis indicates system meets performance requirements. Proceeding with new controller gains.");
                return Ok(pid_controller_gains);
              }
            }
          }
        }
      }
      Err(anyhow::anyhow!("Unable to determine suitable PID gains from UQ analysis"))
    }
}
