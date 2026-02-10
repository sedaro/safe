use anyhow::Result;
use async_trait::async_trait;
use base64::prelude::BASE64_STANDARD;
use simvm::sv::check::Check;
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

use simvm::sv::{combine::TR, combine::TRD, data::Datum, parse::Parse};
use crate::c2::{AutonomyModeMessage, RouterMessage};
use crate::transports::Stream;
use crate::simulation::{self, FileTargetReader, SedaroSimulator};
use crate::kits::stats::{GuassianSet, NormalDistribution};
use crate::kits::stats::StatisticalDistribution;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader};
use simvm::sv::update::Update;
use simvm::sv::pretty::Pretty;

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
impl AutonomyMode<Telemetry, Command> for AttitudeControlAnomalyRecovery {
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
              let pid_controller_gains = (5e-5, 0.0, 2.5e-4, 0.01);
              if let Ok(vetted_gains) = self.uq_pid_gains(pid_controller_gains).await {
                stream
                    .write(RouterMessage::Command { 
                      data: Command::new(vetted_gains),
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
      let init_file_lock = Arc::new(Mutex::new(()));

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
        
        if i % self.concurrency == 0 { // TODO: Undo once configuration interface allows for custom file paths
          // let config_permit = config_sem.clone().acquire_owned().await?;
          inertia_0_0 = inertia_mat_0_0_dist.sample();
          inertia_1_1 = inertia_mat_1_1_dist.sample();
          inertia_2_2 = inertia_mat_2_2_dist.sample();
          x_wheel_inertia = x_wheel_inertia_dist.sample();
          y_wheel_inertia = y_wheel_inertia_dist.sample();
          z_wheel_inertia = z_wheel_inertia_dist.sample();
        }
        
        let success_count_clone = success_count.clone();
        let fail_count_clone = fail_count.clone();
        let init_file_lock_clone = init_file_lock.clone();
        let max_speed_observations_clone = max_speed_observations.clone();
        let max_pointing_error_observations_clone = max_pointing_error_observations.clone();
        let handle = tokio::spawn(async move { // TODO: Try avoiding the spawn?

          let agent_id = "FpKnj2S4YcchDdf2BGX8cfn";
          let eds_path = std::path::PathBuf::from("/Users/sebastianwelsh/Downloads/simulation");
          let results_path = std::path::PathBuf::from(format!("/Users/sebastianwelsh/Development/sedaro/scf/results/uq_run_{}", i));

          // FIXME: RACE: EDS can start up and end up reading the next EDS runs init file if it gets hung up.
          // - random suffix?
          // - accept file name as input
          let _init_file_guard = init_file_lock_clone.lock().await;
          let type_sig = std::fs::read(eds_path.join(format!("data/init_ty_{agent_id}.json")))?;
          let type_sig_str = std::str::from_utf8(&type_sig)?;
          let init_type = TR::parse(type_sig_str).unwrap();
          let init_file_path = eds_path.join(format!("data/init_{agent_id}.bin"));
          let init_bytes = std::fs::read(&init_file_path)?;
          let init_val = dyn_de(&init_type.typ, &init_bytes).unwrap();
          let init_val = TRD::from((init_type.clone(), init_val));
          // println!("Original simulation input Datum: {:?}", init_val.pretty());
          // println!("===============================================================");

          let patch_str = format!("(((({:.15}, {:.15}, {:.15}, {:.15}),),) : (gnc: (\"root!.pid_config\": (float, float, float, float),),))", 5e-5, 123456.0, 2.5e-4, 0.01);
          let patch_trd = TRD::parse(&patch_str).unwrap();
          patch_trd.refn.check(&patch_trd.data).unwrap(); // Get helpful error if patch is malformed
          let init_val = init_val.update(&patch_trd).unwrap();
          let patch_str = format!("((({:.15}, {:.15}, {:.15}),) : (gnc: (\"PTnxx2jZ8vzTJlRwjxyctn.inertia\": float, \"PTnxxBv6drdQlzqJlVvvbC.inertia\": float, \"PTnxxMb7DrlvQKWQsDST4r.inertia\": float),))", 123456.0, 123456.0, 123456.0);
          let patch_trd = TRD::parse(&patch_str).unwrap();
          patch_trd.refn.check(&patch_trd.data).unwrap(); // Get helpful error if patch is malformed
          let init_val = init_val.update(&patch_trd).unwrap();
          let patch_str = format!("((((({:.15}, 0, 0), (0, {:.15}, 0), (0, 0, {:.15})),),) : (gnc: (\"root!.inertia\": {{((float, float, float), (float, float, float), (float, float, float)) | #}},),))", 123456.0, 123456.0, 123456.0);
          let patch_trd = TRD::parse(&patch_str).unwrap();
          // patch_trd.refn.check(&patch_trd.data).unwrap(); // BUG
          let init_val = init_val.update(&patch_trd).unwrap();
          // println!("Modified simulation input Datum: {:?}", init_val.pretty());

          // Write modified initial data back to file
          let init_val = init_val.data;
          let bytes = dyn_ser(&init_type.typ, &init_val).unwrap();
          std::fs::write(&init_file_path, bytes)?;
          drop(_init_file_guard);

          // Clear results
          if results_path.exists() {
            tokio::fs::remove_dir_all(&results_path).await.ok();
          }
          let result = simulator.run(1.0, &results_path).await;
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
          ).await?;
          let frames = reader.read_frames().await?;

          let mut max_speed: f64 = 0.0;
          let mut max_pointing_error: f64 = 0.0;
          let mut pointing_errors = vec![];
          let mut i = 0;
          // (\"PTnYWzsN8kmmrHNtVgCr9G.commanded_torque\": float, \"PTnYWzsN8kmmrHNtVgCr9G.speed\": float, \"PTnYWzsN8kmmrHNtVgCr9G.torque\": {(float, float, float) | #}, \"PTnYWzsQ5nnKDB5NHtWNj3.commanded_torque\": float, \"PTnYWzsQ5nnKDB5NHtWNj3.speed\": float, \"PTnYWzsQ5nnKDB5NHtWNj3.torque\": {(float, float, float) | #}, \"PTnYWzsS6fbQRNkH9rF4r8.commanded_torque\": float, \"PTnYWzsS6fbQRNkH9rF4r8.speed\": float, \"PTnYWzsS6fbQRNkH9rF4r8.torque\": {(float, float, float) | #}, \"PTnYWzsV7sgvClGvcX4WB3.commanded_moment\": float, \"PTnYWzsV7sgvClGvcX4WB3.duty_cycle\": float, \"PTnYWzsV7sgvClGvcX4WB3.torque\": {(float, float, float) | #}, \"PTnYWzsX5DlGhvkFHmPZ9B.commanded_moment\": float, \"PTnYWzsX5DlGhvkFHmPZ9B.duty_cycle\": float, \"PTnYWzsX5DlGhvkFHmPZ9B.torque\": {(float, float, float) | #}, \"PTnYWzsYyXYkNbPWkPPkry.commanded_moment\": float, \"PTnYWzsYyXYkNbPWkPPkry.duty_cycle\": float, \"PTnYWzsYyXYkNbPWkPPkry.torque\": {(float, float, float) | #}, elapsedTime: day, \"root.angular_velocity\": {(float, float, float) | #}, \"root.attitude\": {(float, float, float, float) | #}, \"root.commanded_attitude\": {(float, float, float, float) | #}, \"root.in_shadow\": bool, \"root.magnetic_field\": {(float, float, float) | #}, \"root.pointing_error\": float, \"root.position\": {(float, float, float) | #, eci}, \"root.velocity\": {(float, float, float) | #}, time: day, timeStep: day)
          // speeds: 1, 4, 7
          // pointing_error: 25

          // let FloatValue::F64(x) = f else {
          //   panic!();
          // }

          for frame in &frames {
            let speed = frame.get(1).unwrap().data.as_f64().unwrap();
            if max_speed < speed.abs() { max_speed = speed.abs() }
            let speed = frame.get(4).unwrap().data.as_f64().unwrap();
            if max_speed < speed.abs() { max_speed = speed.abs() }
            let speed = frame.get(7).unwrap().data.as_f64().unwrap();
            if max_speed < speed.abs() { max_speed = speed.abs() }
            if i > frames.len()/2 {
              let pointing_error = frame.get(25).unwrap().data.as_f64().unwrap();
              pointing_errors.push(pointing_error);
              if max_pointing_error < pointing_error.abs() { max_pointing_error = pointing_error.abs() }
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
