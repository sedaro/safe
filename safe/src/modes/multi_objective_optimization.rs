use anyhow::Result;
use argmin::solver::particleswarm::ParticleSwarm;
use async_trait::async_trait;
use crate::c2::{Telemetry, TimedCommand};
use crate::definitions::Activation;
use crate::router::AutonomyMode;
use serde::Serialize;
use simvm::sv::data::Data;
use std::vec;

use simvm::sv::combine::TRD;
use crate::c2::{AutonomyModeMessage, RouterMessage};
use crate::transports::Stream;
use crate::simulation::{FileTargetReader, SedaroSimulator};
use argmin::core::{CostFunction, Error, Executor};
use argmin::core::State;

const SIM_DURATION: f64 = 1.0 / 24.0; // 1 hour in days
const AGENT_ID: &str = "PTnYWzsc2Nhywc8WVS4blm";

const SOC_WEIGHT: f64 = 1000.0;
const DATA_GEN_WEIGHT: f64 = 1.0;
const DATA_DOWN_WEIGHT: f64 = 1.0;
const OUT_OF_BOUNDS_PENALTY: f64 = 1e6;

const MAX_ITERATIONS: u64 = 20;
const TARGET_PERFORMANCE: f64 = -0.8;
const SWARM_SIZE: usize = 5;

fn performance(power_frames: &Vec<TRD>, cdh_frames: &Vec<TRD>) -> f64 {
    let min_soc = power_frames.iter().fold(f64::INFINITY, |min_soc, frame| {
        let soc = frame
            .get_by_field("6VN95ZbK4TNfzYGpr9wGSK.state_of_charge")
            .unwrap()
            .data
            .as_f64()
            .unwrap();
        min_soc.min(soc)
    });
    let max_data_generated = cdh_frames.iter().fold(f64::INFINITY, |max_data_gen, frame| {
        let data_gen = frame
            .get_by_field("root.cumulative_generated_image_data")
            .unwrap()
            .data
            .as_f64()
            .unwrap();
        max_data_gen.min(data_gen)
    });
    let max_data_downlinked = cdh_frames.iter().fold(f64::INFINITY, |max_data_down, frame| {
        let data_down = frame
            .get_by_field("root.cumulative_downlinked_image_data")
            .unwrap()
            .data
            .as_f64()
            .unwrap();
        max_data_down.min(data_down)
    });

    -(min_soc * SOC_WEIGHT + max_data_generated * DATA_GEN_WEIGHT + max_data_downlinked * DATA_DOWN_WEIGHT)
}

fn run_simulation(
    simulator: &SedaroSimulator,
    pointing_schedule: &[(f64, &str)],
    imaging_schedule: &[(f64, &str)],
) -> Result<f64> {
    // Working directory
    println!("-- Starting simulation run --");
    let results_path = tempfile::Builder::new()
        .prefix("simulation_results_")
        .tempdir()?
        .into_path();

    let mut patches = vec![];
    println!("Patching simulation input data...");
    let schedule_str = pointing_schedule
        .iter()
        .map(|(t, s)| format!("({:.15}, \"{}\")", t, s))
        .collect::<Vec<_>>()
        .join(", ");
    let var_details = format!("({AGENT_ID}: (cdh: (\"6VPcwrnbQS6HBHdy3kWtDC.mode_schedule\": [(float, str)],),),)");
    patches.push((var_details, format!("((([{schedule_str}],),),)")));

    let schedule_str = imaging_schedule
        .iter()
        .map(|(t, s)| format!("({:.15}, \"{}\")", t, s))
        .collect::<Vec<_>>()
        .join(", ");
    let var_details = format!("({AGENT_ID}: (cdh: (\"6VPhZLmbZhNnP96c9qbnBw.mode_schedule\": [(float, str)],),),)");
    patches.push((var_details, format!("((([{schedule_str}],),),)")));

    // Clear results dir and run simulation
    println!("Running simulation...");
    if results_path.exists() {
        std::fs::remove_dir_all(&results_path).ok();
    }
    let result = simulator.run_sync(SIM_DURATION, &results_path, Some(&patches));
    match &result {
        Ok(output) => match output.status.success() {
            true => {
                println!("Simulation completed successfully");
            }
            false => {
                return Err(anyhow::anyhow!(
                    "Simulation failed with non-zero exit code: {:?}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        },
        Err(e) => {
            return Err(anyhow::anyhow!("Simulation failed: {:?}", e));
        }
    }

    // Extract frames from local target
    Ok(performance(
        &get_results(AGENT_ID, "power", &results_path)?,
        &get_results(AGENT_ID, "cdh", &results_path)?,
    ))
}

fn get_results(
    agent_id: &str,
    engine: &str,
    results_path: &std::path::PathBuf,
) -> Result<Vec<TRD>> {
    // Extract frames from local target
    let mut reader =
        FileTargetReader::try_from_path(&results_path.join(format!("{agent_id}.{engine}.jsonl")))
            .unwrap();
    let frames = reader.read_frames().unwrap();
    Ok(frames)
}

struct PowerOptimization {
    simulator: SedaroSimulator,
}

impl CostFunction for PowerOptimization {
    type Param = Vec<f64>; // Optimize the start and end time of observation window
    type Output = f64;

    fn cost(&self, param: &Self::Param) -> Result<Self::Output, Error> {
        // We parametrize the schedules on start time and duration of observations. At the
        // start time we simultaneously switch into Nadir pointing and start imaging, and
        // at the end time we switch back to yaw-only sun pointing and stop imaging.
        //
        // TODO: add an offset for imaging to start
        let start_time = param[0] / 86400.0; // Convert seconds to days
        let duration = param[1] / 86400.0; // Convert seconds to days

        // Construct schedule based on the parameters
        let pointing_schedule: Vec<(f64, &str)> = vec![
            (60000., "6VPcrRLY3CrmDrNCpxpVTK"),               // Yaw-only sun
            (60000.0 + start_time, "6VPctJwTStz3JdspSxMgVz"), // Nadir (observation)
            (60000.0 + start_time + duration, "6VPcrRLY3CrmDrNCpxpVTK"),   // Yaw-only sun
        ];

        let imaging_schedule: Vec<(f64, &str)> = vec![
            (60000.0 + start_time, "6VPhZGYSSK9fCDQgYSkdrp"), // Start imaging
            (60000.0 + start_time + duration, "6VPhZGYSSK9fCDQgYSkdrp"),   // FIXME Stop imaging
        ];

        // Run simulation and get performance metric
        // Penalty for out of bounds schedules
        let perf = run_simulation(&self.simulator, &pointing_schedule, &imaging_schedule).unwrap();
        let out_of_bounds = (start_time + duration - SIM_DURATION * 86400.0).min(0.0);
        Ok(perf + OUT_OF_BOUNDS_PENALTY * out_of_bounds)
    }
}

#[derive(Debug, Serialize)]
pub struct MultiObjectiveOptimization {
    name: String,
    priority: u8,
    activation: Activation,
    N: usize,
    concurrency: usize,
    simulator: SedaroSimulator,
}
#[async_trait]
impl AutonomyMode<Telemetry, TimedCommand> for MultiObjectiveOptimization {
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
              println!("MultiObjectiveOptimization activated");

              let problem = PowerOptimization { simulator: self.simulator.clone() };
              let initial_guess = vec![300.0, 600.0]; // Initial guess for start and end times (in seconds)
              let vertices = vec![
                  vec![initial_guess[0], initial_guess[1]], // Initial guess
                  vec![initial_guess[0] + 60.0, initial_guess[1]], // Perturb start time
                  vec![initial_guess[0], initial_guess[1] + 60.0], // Perturb end time
              ];
              // let solver = NelderMead::new(vertices).with_sd_tolerance(1e-3).unwrap();
              let bounds = (vec![0.0, 60.0], vec![3600.0, 3600.0 - 60.0]); // At least 60 seconds of imaging
              let solver = ParticleSwarm::new(bounds, SWARM_SIZE);
              let res = Executor::new(problem, solver)
                  .configure(|state| { state.target_cost(TARGET_PERFORMANCE).max_iters(MAX_ITERATIONS) })
                  .run()
                  .unwrap();
              println!("Optimization best performance: {:?}", res.state.best_cost);
              let best_param = &res.state.get_best_param().expect("No best parameters found").position;
              println!(
                  "Optimization best parameters (start_elapsed_time, duration (s)): ({:?}, {:?})",
                  best_param[0], best_param[1]);

              // stream
              //   .write(RouterMessage::Command { 
              //     data: TimedCommand::Scheduled {
              //       cmd: Command::CaptureImage,
              //       gps_time: utc_mjd_to_gps(),
              //     },
              //     nonce: new_nonce,
              //   })
              //   .await?;
            },
            AutonomyModeMessage::Inactive => {
              active = false;
              println!("MultiObjectiveOptimization deactivated");
            },
            AutonomyModeMessage::Telemetry(_telemetry) => {
              // println!("MultiObjectiveOptimization received telemetry {:?}", _telemetry);
              // Ignore telemetry for now
            },
          }
        }
      }
    }
}

impl MultiObjectiveOptimization {
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
}
