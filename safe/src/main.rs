mod c2;
mod config;
mod definitions;
mod kits;
mod observability;
mod router;
mod transports;
mod simulation;

use anyhow::Result;
use async_trait::async_trait;
use base64::prelude::BASE64_STANDARD;
use c2::{Command, Telemetry};
use config::Config;
use definitions::{Activation, Expr, Value, Variable};
use figment::providers::{Env, Format, Serialized, Yaml};
use figment::Figment;
use observability as obs;
use router::{AutonomyMode, Router};
use serde::Serialize;
use simvm::sv::data::{Data, FloatValue};
use simvm::sv::ser_de::{dyn_de, dyn_ser};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::util::SubscriberInitExt;
use std::sync::Arc;
use std::vec;
use tokio::sync::{Mutex, broadcast, mpsc};
use tracing::{error, info, warn};
use tokio::sync::Semaphore;
use ordered_float::OrderedFloat;
use tracing_subscriber::Layer;
use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;
use tracing::Instrument;
use time;
use base64::Engine;

use simvm::sv::{combine::TR, data::Datum, parse::Parse, pretty::Pretty};
use crate::transports::Transport;
use crate::transports::TransportHandle;
use crate::transports::{MpscTransport, Stream, TcpTransport, UnixTransport};
use crate::simulation::SedaroSimulator;
use crate::kits::stats::{GuassianSet, NormalDistribution};
use crate::kits::stats::StatisticalDistribution;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader, AsyncSeekExt, SeekFrom};
use std::time::Duration;

#[derive(Debug, Serialize)]
struct CollisionAvoidanceAutonomyMode {
    name: String,
    priority: u8,
    activation: Option<Activation>,
}
#[async_trait]
impl AutonomyMode for CollisionAvoidanceAutonomyMode {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn priority(&self) -> u8 {
        self.priority
    }
    fn activation(&self) -> Option<Activation> {
        self.activation.clone()
    }
    async fn run(
        &mut self,
        mut rx_telem: broadcast::Receiver<Telemetry>,
        tx_command: mpsc::Sender<Command>,
        active: Arc<tokio::sync::Mutex<bool>>,
    ) -> Result<()> {
        loop {
            // tx_command
            //     .send(Command {
            //         commanded_attitude: vec![0, 0, 1],
            //         thrust: rand::random::<u8>() % 26 + 65,
            //     })
            //     .await?;
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            // if let Ok(telemetry) = rx_telem.recv().await {
            //     let active = active.lock().await;
            //     info!(
            //         "{} [{}] received telemetry: {:?}",
            //         self.name(),
            //         active,
            //         telemetry
            //     );
            // }
        }
    }
}

#[derive(Debug, Serialize)]
struct NominalOperationsAutonomyMode {
    name: String,
    priority: u8,
    activation: Option<Activation>,
}
#[async_trait]
impl AutonomyMode for NominalOperationsAutonomyMode {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn priority(&self) -> u8 {
        self.priority
    }
    fn activation(&self) -> Option<Activation> {
        self.activation.clone()
    }
    async fn run(
        &mut self,
        mut rx_telem: broadcast::Receiver<Telemetry>,
        tx_command: mpsc::Sender<Command>,
        active: Arc<tokio::sync::Mutex<bool>>,
    ) -> Result<()> {
        loop {
            // if {
            //   let active = active.lock().await;
            //   *active
            // } {
            //   tx_command
            //       .send(Command {
            //           commanded_attitude: vec![0, 1, 0],
            //           thrust: 0,
            //       })
            //       .await?;
            // }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            // if let Ok(telemetry) = rx_telem.recv().await {
            //     let active = active.lock().await;
            //     info!(
            //         "{} [{}] received telemetry: {:?}",
            //         self.name(),
            //         active,
            //         telemetry
            //     );
            // }
        }
    }
}

#[derive(Debug, Serialize)]
struct GenericUncertaintyQuantificationAutonomyMode {
    name: String,
    priority: u8,
    activation: Option<Activation>,
    N: usize,
    concurrency: usize,
    simulator: SedaroSimulator,
}
#[async_trait]
impl AutonomyMode for GenericUncertaintyQuantificationAutonomyMode {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn priority(&self) -> u8 {
        self.priority
    }
    fn activation(&self) -> Option<Activation> {
        self.activation.clone()
    }
    async fn run(
        &mut self,
        mut rx_telem: broadcast::Receiver<Telemetry>,
        tx_command: mpsc::Sender<Command>,
        active: Arc<tokio::sync::Mutex<bool>>, // TODO: Make this an Event that can be efficiently waited on
    ) -> Result<()> {
      
      let mut first = true;
      loop {

        // FIXME: Very hacky - fix
        while {
          let active = active.lock().await;
          !first && *active
        } {
          tokio::time::sleep(Duration::from_millis(100)).await;
        }
        first = false;
        while {
          let active = active.lock().await;
          !*active
        } {
          tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let pid_controller_gains = (5e-5, 0.0, 2.5e-4, 0.01);
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

        let mut inertia_mat_0_0_dist = NormalDistribution::new(inertia_0_0, inertia_0_0 * ((2.5 / 3.0) / 100.0), 10);
        let mut inertia_mat_1_1_dist = NormalDistribution::new(inertia_1_1, inertia_1_1 * ((2.5 / 3.0) / 100.0), 11);
        let mut inertia_mat_2_2_dist = NormalDistribution::new(inertia_2_2, inertia_2_2 * ((2.5 / 3.0) / 100.0), 12);
        let mut x_wheel_inertia_dist = NormalDistribution::new(x_wheel_inertia, x_wheel_inertia * ((2.5 / 3.0) / 100.0), 13);
        let mut y_wheel_inertia_dist = NormalDistribution::new(y_wheel_inertia, y_wheel_inertia * ((2.5 / 3.0) / 100.0), 14);
        let mut z_wheel_inertia_dist = NormalDistribution::new(z_wheel_inertia, z_wheel_inertia * ((2.5 / 3.0) / 100.0), 15);
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

            // FIXME: RACE: EDS can start up and end up reading the next EDS runs init file if it gets hung up.
            // - random suffix?
            // - accept file name as input
            let _init_file_guard = init_file_lock_clone.lock().await;
            let init_type = TR::parse("(gnc: (\"$as_Position.eci_HGDv5HpfFwcMfs3XpDM5Cl7\": {(float, float, float) | #, eci}, \"PTncgtNhFd5whhVCp34ykX.absorptivity\": float, \"PTncgtNhFd5whhVCp34ykX.area\": float, \"PTncgtNhFd5whhVCp34ykX.centroid\": {(float, float, float) | #}, \"PTncgtNhFd5whhVCp34ykX.diffuse_reflectivity\": float, \"PTncgtNhFd5whhVCp34ykX.orientation\": {(float, float, float) | #}, \"PTncgtNhFd5whhVCp34ykX.specular_reflectivity\": float, \"PTncgtNkFCJ3XVXBWtl6Dq.absorptivity\": float, \"PTncgtNkFCJ3XVXBWtl6Dq.area\": float, \"PTncgtNkFCJ3XVXBWtl6Dq.centroid\": {(float, float, float) | #}, \"PTncgtNkFCJ3XVXBWtl6Dq.diffuse_reflectivity\": float, \"PTncgtNkFCJ3XVXBWtl6Dq.orientation\": {(float, float, float) | #}, \"PTncgtNkFCJ3XVXBWtl6Dq.specular_reflectivity\": float, \"PTncgtNmCND8VwFwtF2gfQ.absorptivity\": float, \"PTncgtNmCND8VwFwtF2gfQ.area\": float, \"PTncgtNmCND8VwFwtF2gfQ.centroid\": {(float, float, float) | #}, \"PTncgtNmCND8VwFwtF2gfQ.diffuse_reflectivity\": float, \"PTncgtNmCND8VwFwtF2gfQ.orientation\": {(float, float, float) | #}, \"PTncgtNmCND8VwFwtF2gfQ.specular_reflectivity\": float, \"PTncgtNp9vVdj3mppRd32k.absorptivity\": float, \"PTncgtNp9vVdj3mppRd32k.area\": float, \"PTncgtNp9vVdj3mppRd32k.centroid\": {(float, float, float) | #}, \"PTncgtNp9vVdj3mppRd32k.diffuse_reflectivity\": float, \"PTncgtNp9vVdj3mppRd32k.orientation\": {(float, float, float) | #}, \"PTncgtNp9vVdj3mppRd32k.specular_reflectivity\": float, \"PTncgtNr6jxLJvWNX5MDpv.absorptivity\": float, \"PTncgtNr6jxLJvWNX5MDpv.area\": float, \"PTncgtNr6jxLJvWNX5MDpv.centroid\": {(float, float, float) | #}, \"PTncgtNr6jxLJvWNX5MDpv.diffuse_reflectivity\": float, \"PTncgtNr6jxLJvWNX5MDpv.orientation\": {(float, float, float) | #}, \"PTncgtNr6jxLJvWNX5MDpv.specular_reflectivity\": float, \"PTncgtNt73rhBxzTXGtlXK.absorptivity\": float, \"PTncgtNt73rhBxzTXGtlXK.area\": float, \"PTncgtNt73rhBxzTXGtlXK.centroid\": {(float, float, float) | #}, \"PTncgtNt73rhBxzTXGtlXK.diffuse_reflectivity\": float, \"PTncgtNt73rhBxzTXGtlXK.orientation\": {(float, float, float) | #}, \"PTncgtNt73rhBxzTXGtlXK.specular_reflectivity\": float, \"PTncgtNw9cCwtmPFR8k6fj.id\": u128, \"PTncgtNw9cCwtmPFR8k6fj.inertia\": float, \"PTncgtNw9cCwtmPFR8k6fj.orientation\": {(float, float, float) | #}, \"PTncgtNw9cCwtmPFR8k6fj.rated_momentum\": float, \"PTncgtNw9cCwtmPFR8k6fj.rated_torque\": float, \"PTncgtNw9cCwtmPFR8k6fj.speed\": float, \"PTncgtNw9cCwtmPFR8k6fj.torque\": {(float, float, float) | #}, \"PTncgtNy9WYYK6zL6GKLcG.id\": u128, \"PTncgtNy9WYYK6zL6GKLcG.inertia\": float, \"PTncgtNy9WYYK6zL6GKLcG.orientation\": {(float, float, float) | #}, \"PTncgtNy9WYYK6zL6GKLcG.rated_momentum\": float, \"PTncgtNy9WYYK6zL6GKLcG.rated_torque\": float, \"PTncgtNy9WYYK6zL6GKLcG.speed\": float, \"PTncgtNy9WYYK6zL6GKLcG.torque\": {(float, float, float) | #}, \"PTncgtP28MCmBbCY3SStDQ.id\": u128, \"PTncgtP28MCmBbCY3SStDQ.inertia\": float, \"PTncgtP28MCmBbCY3SStDQ.orientation\": {(float, float, float) | #}, \"PTncgtP28MCmBbCY3SStDQ.rated_momentum\": float, \"PTncgtP28MCmBbCY3SStDQ.rated_torque\": float, \"PTncgtP28MCmBbCY3SStDQ.speed\": float, \"PTncgtP28MCmBbCY3SStDQ.torque\": {(float, float, float) | #}, \"PTncgtP43vT45yhYKVBQzp.commanded_moment\": float, \"PTncgtP43vT45yhYKVBQzp.id\": u128, \"PTncgtP43vT45yhYKVBQzp.orientation\": {(float, float, float) | #}, \"PTncgtP43vT45yhYKVBQzp.rated_magnetic_moment\": float, \"PTncgtP65nt3vBfcxfW3rB.commanded_moment\": float, \"PTncgtP65nt3vBfcxfW3rB.id\": u128, \"PTncgtP65nt3vBfcxfW3rB.orientation\": {(float, float, float) | #}, \"PTncgtP65nt3vBfcxfW3rB.rated_magnetic_moment\": float, \"PTncgtP7zM3lYmRqrNRyrL.commanded_moment\": float, \"PTncgtP7zM3lYmRqrNRyrL.id\": u128, \"PTncgtP7zM3lYmRqrNRyrL.orientation\": {(float, float, float) | #}, \"PTncgtP7zM3lYmRqrNRyrL.rated_magnetic_moment\": float, \"root!.angular_acceleration\": {(float, float, float) | #}, \"root!.angular_velocity\": {(float, float, float) | #}, \"root!.attitude\": {(float, float, float, float) | #}, \"root!.elapsedTime\": day, \"root!.inertia\": {((float, float, float), (float, float, float), (float, float, float)) | #}, \"root!.mass\": float, \"root!.pid_config\": (float, float, float, float), \"root!.position\": {(float, float, float) | #, eci}, \"root!.time\": day, \"root!.timeStep\": day, \"root!.velocity\": {(float, float, float) | #}),)").unwrap();
            // Inertia matrix is 74
            // Wheel inertial is 38, 45, 52
            // PID: 76
            let bytes = std::fs::read("/Users/sebastianwelsh/Development/sedaro/scf/simulation/data/init_BXSc5nBDFLXzVVQH3ZyGPqk.bin")?; // FIXME
            let init_val = dyn_de(&init_type.typ, &bytes).unwrap();
            // println!("Original simulation input Datum: {:?}", init_val.pretty());
            let gnc = init_val.get(0).unwrap();
            let mut gnc = gnc.clone();
            let pid_config = gnc.get(76).unwrap();
            let mut pid_config = pid_config.clone();
            pid_config.set(0, Datum::Float(FloatValue::F64(OrderedFloat(pid_controller_gains.0)))).unwrap();
            pid_config.set(1, Datum::Float(FloatValue::F64(OrderedFloat(pid_controller_gains.1)))).unwrap();
            pid_config.set(2, Datum::Float(FloatValue::F64(OrderedFloat(pid_controller_gains.2)))).unwrap();
            pid_config.set(3, Datum::Float(FloatValue::F64(OrderedFloat(pid_controller_gains.3)))).unwrap();
            gnc.set(76, pid_config).unwrap();
            gnc.set(38, Datum::Float(FloatValue::F64(OrderedFloat(x_wheel_inertia)))).unwrap();
            gnc.set(45, Datum::Float(FloatValue::F64(OrderedFloat(y_wheel_inertia)))).unwrap();
            gnc.set(52, Datum::Float(FloatValue::F64(OrderedFloat(z_wheel_inertia)))).unwrap();
            let inertia_mat = gnc.get(74).unwrap();
            let mut inertia_mat = inertia_mat.clone();
            let row0 = inertia_mat.get(0).unwrap();
            let mut row0 = row0.clone();
            row0.set(0, Datum::Float(FloatValue::F64(OrderedFloat(inertia_0_0)))).unwrap();
            inertia_mat.set(0, row0).unwrap();
            let row1 = inertia_mat.get(1).unwrap();
            let mut row1 = row1.clone();
            row1.set(1, Datum::Float(FloatValue::F64(OrderedFloat(inertia_1_1)))).unwrap();
            inertia_mat.set(1, row1).unwrap();
            let row2 = inertia_mat.get(2).unwrap();
            let mut row2 = row2.clone();
            row2.set(2, Datum::Float(FloatValue::F64(OrderedFloat(inertia_2_2)))).unwrap();
            inertia_mat.set(2, row2).unwrap();
            gnc.set(74, inertia_mat).unwrap();
            let mut init_val = init_val.clone();
            init_val.set(0, gnc).unwrap(); // FIXME: Ugly
            let bytes = dyn_ser(&init_type.typ, &init_val).unwrap();
            // debug!("Modified simulation input Datum: {:?}", init_val);
            std::fs::write("/Users/sebastianwelsh/Development/sedaro/scf/simulation/data/init_BXSc5nBDFLXzVVQH3ZyGPqk.bin", bytes)?; // FIXME
            drop(_init_file_guard);

            let results_path = std::path::PathBuf::from(format!("/Users/sebastianwelsh/Development/sedaro/scf/results/uq_run_{}", i));
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
                  },
                }
              },
              Err(e) => {
                warn!("Simulation failed: {:?}", e);
                *fail_count_clone.lock().await += 1.0;
              },
            }

            let mut frames: Vec<Datum> = vec![];
            if let Ok(file) = File::open(results_path.join("PTng49cD7jbNrXZMZQJVmW.gnc.jsonl")).await {
              // Seek to end of file
              let mut reader = BufReader::new(file);
              let mut line = String::new();
              let mut parsed_type: Option<TR> = None;
              while let Ok(bytes_read) = reader.read_line(&mut line).await {
                if bytes_read == 0 {
                  break; // Reached end of file
                }
                if let Ok(config) = serde_json::from_str::<simulation::FileTargetConfigEntry>(&line) {
                  parsed_type = Some(TR::parse(&config.data.type_).unwrap());
                } else if let Ok(entry) = serde_json::from_str::<simulation::FileTargetFrameEntry>(&line) {
                  if let Some(parsed) = &parsed_type {
                    let frame_bytes = BASE64_STANDARD.decode(&entry.data.frame).unwrap();
                    match dyn_de(&parsed.typ, &frame_bytes) {
                      Ok(val) => {
                        frames.push(val);
                      },
                      Err(e) => {
                        warn!("Simulation {} frame deserialization error: {:?}", i, e);
                      },
                    }
                  }
                }
                line.clear();
              }
            }
            let mut max_speed: f64 = 0.0;
            let mut max_pointing_error: f64 = 0.0;
            let mut pointing_errors = vec![];
            let mut i = 0;
            // (\"PTnYWzsN8kmmrHNtVgCr9G.commanded_torque\": float, \"PTnYWzsN8kmmrHNtVgCr9G.speed\": float, \"PTnYWzsN8kmmrHNtVgCr9G.torque\": {(float, float, float) | #}, \"PTnYWzsQ5nnKDB5NHtWNj3.commanded_torque\": float, \"PTnYWzsQ5nnKDB5NHtWNj3.speed\": float, \"PTnYWzsQ5nnKDB5NHtWNj3.torque\": {(float, float, float) | #}, \"PTnYWzsS6fbQRNkH9rF4r8.commanded_torque\": float, \"PTnYWzsS6fbQRNkH9rF4r8.speed\": float, \"PTnYWzsS6fbQRNkH9rF4r8.torque\": {(float, float, float) | #}, \"PTnYWzsV7sgvClGvcX4WB3.commanded_moment\": float, \"PTnYWzsV7sgvClGvcX4WB3.duty_cycle\": float, \"PTnYWzsV7sgvClGvcX4WB3.torque\": {(float, float, float) | #}, \"PTnYWzsX5DlGhvkFHmPZ9B.commanded_moment\": float, \"PTnYWzsX5DlGhvkFHmPZ9B.duty_cycle\": float, \"PTnYWzsX5DlGhvkFHmPZ9B.torque\": {(float, float, float) | #}, \"PTnYWzsYyXYkNbPWkPPkry.commanded_moment\": float, \"PTnYWzsYyXYkNbPWkPPkry.duty_cycle\": float, \"PTnYWzsYyXYkNbPWkPPkry.torque\": {(float, float, float) | #}, elapsedTime: day, \"root.angular_velocity\": {(float, float, float) | #}, \"root.attitude\": {(float, float, float, float) | #}, \"root.commanded_attitude\": {(float, float, float, float) | #}, \"root.in_shadow\": bool, \"root.magnetic_field\": {(float, float, float) | #}, \"root.pointing_error\": float, \"root.position\": {(float, float, float) | #, eci}, \"root.velocity\": {(float, float, float) | #}, time: day, timeStep: day)
            // speeds: 1, 4, 7
            // pointing_error: 24

            for frame in &frames {
              let speed = match frame.get(1).unwrap().as_float().unwrap() {
                FloatValue::F64(f) => f,
                FloatValue::F32(f) => panic!("Got f32 but expected f64"),
              };
              if max_speed < speed.abs() { max_speed = speed.abs() }
              let speed = match frame.get(4).unwrap().as_float().unwrap() {
                FloatValue::F64(f) => f,
                FloatValue::F32(f) => panic!("Got f32 but expected f64"),
              };
              if max_speed < speed.abs() { max_speed = speed.abs() }
              let speed = match frame.get(7).unwrap().as_float().unwrap() {
                FloatValue::F64(f) => f,
                FloatValue::F32(f) => panic!("Got f32 but expected f64"),
              };
              if max_speed < speed.abs() { max_speed = speed.abs() }
              if i > frames.len()/2 {
                let pointing_error = match frame.get(24).unwrap().as_float().unwrap() {
                  FloatValue::F64(f) => f,
                  FloatValue::F32(f) => panic!("Got f32 but expected f64"),
                };
                pointing_errors.push(pointing_error);
                if max_pointing_error < pointing_error.abs() { max_pointing_error = pointing_error.abs() }
              }
              // println!("{}", frame.pretty());
              i += 1;
            }        
            let average_error =pointing_errors.iter().sum::<OrderedFloat<f64>>().0 / (pointing_errors.len() as f64);
            println!("{} {} {}", frames.len(), max_speed, average_error);
            max_speed_observations_clone.lock().await.add(max_speed);
            max_pointing_error_observations_clone.lock().await.add(average_error);
            result
          }.in_current_span());
          
          handles.push(handle);
          
          // If at 5% increments
          if (self.N / 20) > 0 && (i + 1) % (self.N / 20) == 0 {
            let s_count = success_count.clone();
            let s_count = s_count.lock().await.clone();
            let f_count = fail_count.clone();
            let f_count = f_count.lock().await.clone();
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
                  tx_command
                    .send(Command {
                        set_pid_controller_gains: pid_controller_gains,
                    })
                    .await?;
                }
              }
            }
          }
        }
      }
    }
}

// TODO: Overhaul this handler and the overall Client<>SAFE interface
async fn handle_client(
    mut stream: impl Stream<String, String>,
    mut router_stream: impl Stream<Command, Telemetry>,
) {
    if let Ok(msg) = stream.read().await {
        if let Ok(logs_request) = serde_json::from_str::<c2::LogsRequest>(&msg) { // TODO: Rewrite this once tested and more tightly couple to observability system
            // Tail safe.log.<date> files and send over stream
            let log_dir = std::path::Path::new("./logs");
            let day_date = time::OffsetDateTime::now_utc().date().to_string();
            let file_prefix = match logs_request.mode {
              Some(mode) => mode,
              None => "safe".to_string(),
            };
            let log_file = format!("{}.log.{}", file_prefix, day_date);
            // TODO: Return error response if log stream (i.e. file) doesn't exist
            if let Ok(mut file) = File::open(log_dir.join(log_file)).await {
              // Seek to end of file
              let _ = file.seek(SeekFrom::End(0)).await;
              let mut reader = BufReader::new(file);
              let mut line = String::new();

              loop {
                let curr_day_date = time::OffsetDateTime::now_utc().date().to_string();
                if day_date != curr_day_date {
                  // Day has changed, switch to new log file after sending the rest of current file
                  while let Ok(bytes_read) = reader.read_line(&mut line).await {
                    if bytes_read == 0 {
                      break; // Reached end of file
                    }
                    if let Err(_) = stream.write(line.trim().to_string()).await {
                      return; // Client disconnected
                    }
                    line.clear();
                  }
                  let log_file = format!("{}.log.{}", file_prefix, curr_day_date);
                  if let Ok(new_file) = File::open(log_dir.join(log_file)).await {
                    file = new_file;
                    reader = BufReader::new(file);
                    line.clear();
                  }
                }
                match reader.read_line(&mut line).await {
                  Ok(0) => {
                    // No new data, wait and try again
                    tokio::time::sleep(Duration::from_millis(100)).await;
                  }
                  Ok(_) => {
                    if let Err(_) = stream.write(line.trim().to_string()).await {
                      break; // Client disconnected
                    }
                    line.clear();
                  }
                  Err(_) => break,
                }
              }
            }
        } else if let Ok(tlm) = serde_json::from_str::<c2::Telemetry>(&msg) {
            router_stream.write(tlm).await.ok();
        } else if let Ok(_) = serde_json::from_str::<c2::CommandsRequest>(&msg) {
          loop {
              // TODO: Figure out how to break out of this loop when client hangs up!
              match router_stream.read().await {
                  Ok(cmd) => {
                      let msg = serde_json::to_string(&cmd).unwrap();
                      stream.write(msg).await.ok();
                  }
                  Err(_) => break,
              }
          }
        } else {
            error!("Unknown client message: {}", msg);
        }
        
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config: Config = Figment::new()
        // Start with defaults
        .merge(Serialized::defaults(Config::default()))
        // Load from config file (optional)
        .merge(Yaml::file("safe.yaml").nested())
        // Override with environment variables
        // Format: SAFE__ROUTER__MAX_AUTONOMY_MODES=128
        .merge(Env::prefixed("SAFE__").split("__"))
        .extract()?;

    // FIXME: Move these to a AM registration step
    let (non_blocking, _guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily("./logs", "safe.log"));
    let (non_blocking_a, _guard_a) =
      tracing_appender::non_blocking(tracing_appender::rolling::daily("./logs", "AttitudeControlAnomalyRecovery.log"));
    let (non_blocking_b, _guard_b) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily("./logs", "NominalOps.log"));
    tracing_subscriber::registry()
      .with(
        tracing_subscriber::fmt::layer()
          .with_writer(non_blocking)
          .with_target(false)
          .with_level(true)
          .json()
      )
      .with(
        tracing_subscriber::fmt::layer()
          .with_writer(non_blocking_a)
          .with_target(false)
          .with_level(true)
          .json()
          .with_filter(EnvFilter::new("[{autonomy_mode=AttitudeControlAnomalyRecovery}]=trace"))
      )
      .with(
        tracing_subscriber::fmt::layer()
          .with_writer(non_blocking_b)
          .with_target(false)
          .with_level(true)
          .json()
          .with_filter(EnvFilter::new("[{autonomy_mode=NominalOps}]=trace"))
      )
      .init();

    info!("SAFE is in start up.");
    println!("SAFE is in start up.");

    let observability = Arc::new(obs::ObservabilitySubsystem::new(None));
    let observability_clone = observability.clone();
    let config_clone = config.clone();
    tokio::spawn(async move {
        if let Err(e) = observability_clone.run(&config_clone).await {
            warn!("Observability error: {}", e);
        }
    });

    // let c2_to_router_telemetry_transport: MpscTransport<Telemetry, Command> = MpscTransport::new(config.router.telem_channel_buffer_size);
    let c2_to_router_telemetry_transport: TcpTransport<Telemetry, Command> =
        TcpTransport::new("127.0.0.1", 8000).await?;
    // let c2_to_router_telemetry_transport: UnixTransport<Telemetry, Command> = UnixTransport::new("/tmp/my.sock").await?;
    let handle = c2_to_router_telemetry_transport.handle();

    // Create router and then register Modes to it.
    // This allows for us to dynamically create and destroy Modes while running.
    let mut router = Router::new(
        c2_to_router_telemetry_transport,
        observability.clone(),
        &config,
    );

    let mode = CollisionAvoidanceAutonomyMode {
        name: "CollisionAvoidance".to_string(),
        priority: 1,
        activation: Some(Activation::Hysteretic {
            enter: Expr::Not(Box::new(Expr::GreaterThan(
                Box::new(Expr::Term(Variable::Float64(Value::TelemetryRef(
                    "proximity_m".to_string(),
                )))),
                Box::new(Expr::Term(Variable::Float64(Value::Literal(100.0)))),
            ))),
            exit: Expr::GreaterThan(
                Box::new(Expr::Term(Variable::Float64(Value::TelemetryRef(
                    "proximity_m".to_string(),
                )))),
                Box::new(Expr::Term(Variable::Float64(Value::Literal(150.0)))),
            ),
        }),
    };
    let mode = GenericUncertaintyQuantificationAutonomyMode {
        name: "AttitudeControlAnomalyRecovery".to_string(),
        priority: 1,
        activation: Some(Activation::Hysteretic {
            enter: Expr::GreaterThan(
                Box::new(Expr::Term(Variable::Float64(Value::TelemetryRef(
                    "pointing_error".to_string(),
                )))),
                Box::new(Expr::Term(Variable::Float64(Value::Literal(2.0)))),
            ),
            exit: Expr::Not(Box::new(Expr::GreaterThan(
                Box::new(Expr::Term(Variable::Float64(Value::TelemetryRef(
                    "pointing_error".to_string(),
                )))),
                Box::new(Expr::Term(Variable::Float64(Value::Literal(2.0)))),
            ))),
        }),
        N: 100,
        concurrency: 12,
        simulator: SedaroSimulator::new(
          std::path::PathBuf::from("/Users/sebastianwelsh/Development/sedaro/scf/simulation"),
          "./target/release/main",
          None,
        ),
    };
    router.register_autonomy_mode(mode, &config);

    let mode = NominalOperationsAutonomyMode {
        name: "NominalOps".to_string(),
        priority: 0,
        activation: Some(Activation::Immediate(Expr::Term(Variable::Bool(
            Value::Literal(true),
        )))),
    };
    router.register_autonomy_mode(mode, &config);

    let config_clone = config.clone();
    tokio::spawn(async move {
        if let Err(e) = router.run(&config_clone).await {
            warn!("Router error: {}", e); // TODO: This needs to be more than a warning
        }
    });

    // let mut transport: UnixTransport<String, String> = UnixTransport::new("/tmp/safe.sock").await?;
    let mut transport: TcpTransport<String, String> = TcpTransport::new("127.0.0.1", 8001).await?;
    println!("SAFE C2 listening on {}:{}", "127.0.0.1", 8001);
    tokio::spawn(async move {
        loop {
            match transport.accept().await {
                Ok(stream) => {
                    let c2_telem_stream = handle.connect().await.unwrap();
                    tokio::spawn(async move {
                        handle_client(stream, c2_telem_stream).await;
                    });
                }
                Err(e) => eprintln!("Connection error: {}", e),
            }
        }
    });

    tokio::signal::ctrl_c().await?;
    info!("Shutting down");

    Ok(())
}

/*
On deck:
Logs
Config changes
 */

/*
- Have a rust-native autonomy mode or two
- Timeouts on EDSs that are taking too long
- Implement a way to have background modes which are alerted when they are activated/deactivated
- Utilities for debouncing or filtering out potentially noisy telemetry inputs to get a confident reading.  Make this part of activations for modes.
- Mode transition command purging
- Try to compile it for Raspberry PI and STM MCU
- Focus on the EDS integration piece
- CLI to issue commands over unix socket to Config and C2 interfaces
- Integrate redb
- Is it important to guarantee that Modes can't issue commands when Router logic would deactivate them?  Do we need to work out the races here or is this acceptable?
- Add resiliency and reconnect to Transports which can theoretically drop connections (TCP, Unix sockets, etc.)
 */

/*
Known issues:
- Can start the Mode and also hold it as mutable because the run awaits indefinitely
- Need to better Activation interpreter
 */

/*
Define ontology
Ontology should be fully persisted in redb
Ontology should support variables which can be updated via config interface
 */

/*
Think hard about how SAFE comes up if state already exists!!!
 */

/*
How is routing logic defined?  Part of the Ontology?  Ask team.
Needs to be able to implement arbitrary logic as rust code as a fall back.
 */
