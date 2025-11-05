use rand::{self, SeedableRng};
use rand_distr::{Distribution, Normal};
use serde::Serialize;

// TODO: Revisit.  Pulled from Phase I prototype

#[derive(Debug, Serialize)]
pub struct Sample {
    cpu_power: f64,
    solar_cell_max_power_current: f64,
    // position: Vec<f64>,
    // velocity: Vec<f64>,
    // mass: f64,
    // isp: f64,
}

#[derive(Debug, Serialize)]
struct RunSummary {
    terminated_early: bool,
    message: Option<String>,
    sample: Sample,
    final_soc: Option<f64>,
}

#[derive(Debug, Serialize)]
struct MonteCarloSummary {
    iterations: usize,
    run_time_sec: f64,
    failed_proc_count: usize,
    early_term_count: usize,
    early_term_rate: f64,
    summaries: Vec<RunSummary>,
}

pub fn generate_sample(prior_sample: &Sample, seed: usize) -> Sample {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed as u64);

    // CPU Power Draw
    let sigma = 1.0 / 2.0;
    let normal = Normal::new(0.0, sigma).expect("Could not create normal distribution.");
    let cpu_power_sample = prior_sample.cpu_power + normal.sample(&mut rng);

    // Solar Cell Max Power Current
    let sigma = 0.05 / 2.0;
    let normal = Normal::new(0.0, sigma).expect("Could not create normal distribution.");
    let solar_cell_max_power_current_sample = prior_sample.solar_cell_max_power_current + normal.sample(&mut rng);

    Sample {
        cpu_power: cpu_power_sample,
        solar_cell_max_power_current: solar_cell_max_power_current_sample,
    }
}
