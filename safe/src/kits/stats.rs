use std::fmt::Display;

use rand::{self, SeedableRng};
use rand_distr::{Distribution, Normal};

// TODO: Brad to architect this module

pub trait StatisticalDistribution<T> {
    fn sample(&mut self) -> T;
}

#[derive(Debug)]
pub struct NormalDistribution {
    rng: rand::rngs::StdRng,
    distribution: Normal<f64>,
}
impl NormalDistribution {
    pub fn new(mean: f64, std_dev: f64, seed: usize) -> Self {
        let rng = rand::rngs::StdRng::seed_from_u64(seed as u64);
        let distribution = Normal::new(mean, std_dev).expect("Could not create normal distribution.");
        NormalDistribution { rng, distribution }
    }
}
impl StatisticalDistribution<f64> for NormalDistribution {
    fn sample(&mut self) -> f64 {
        self.distribution.sample(&mut self.rng)
    }
}

#[derive(Debug)]
pub struct GuassianSet {
    pub values: Vec<f64>,
}
// TODO: Brad to review
impl GuassianSet {
    pub fn new() -> Self {
        GuassianSet { values: Vec::new() }
    }
    pub fn add(&mut self, value: f64) {
        self.values.push(value);
    }
    pub fn mean(&self) -> Option<f64> {
        if self.values.is_empty() {
            return None;
        }
        let sum: f64 = self.values.iter().sum();
        Some(sum / (self.values.len() as f64))
    }
    pub fn variance(&self) -> Option<f64> {
        if self.values.len() < 2 {
            return None;
        }
        let mean = self.mean().unwrap();
        let var_sum: f64 = self
            .values
            .iter()
            .map(|value| (value - mean).powi(2))
            .sum();
        Some(var_sum / ((self.values.len() - 1) as f64)) // Bessel's correction
    }
    pub fn std_dev(&self) -> Option<f64> {
        self.variance().map(|var| var.sqrt())
    }
    pub fn std_err(&self) -> Option<f64> {
        match (self.std_dev(), self.values.len()) {
            (Some(std_dev), n) if n > 0 => Some(std_dev / (n as f64).sqrt()),
            _ => None,
        }
    }
}
impl Display for GuassianSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.mean(), self.std_dev(), self.std_err()) {
            (Some(mean), Some(std_dev), Some(std_err)) => {
                write!(
                    f,
                    "mean: {:.6}, std dev: {:.6}, std err: {:.6}",
                    mean, std_dev, std_err
                )
            }
            _ => write!(f, "Insufficient data to compute statistics."),
        }
    }
}

mod tests {
    use super::*;

    fn approx_eq(a: f64, b: f64, epsilon: f64) -> bool {
        (a - b).abs() < epsilon
    }

    #[test]
    fn test_guassian_distribution() {
        let mut dist = NormalDistribution::new(10.0, 0.5, 7);
        let initial_sample = dist.sample();
        assert_eq!(initial_sample, 9.478211686635008);
        assert_ne!(initial_sample, dist.sample());

        let mut dist = NormalDistribution::new(10.0, 0.5, 7);
        assert_eq!(dist.sample(), initial_sample);

        let mut dist = NormalDistribution::new(10.0, 0.5, 8);
        assert_ne!(dist.sample(), initial_sample);
    }

    #[test]
    fn test_guassian_set() {
        let mut obs_set = GuassianSet::new();
        obs_set.add(1.0);
        obs_set.add(2.0);
        obs_set.add(3.0);
        obs_set.add(4.0);
        assert!(approx_eq(obs_set.mean().unwrap(), 2.5, 1e-9));
        assert!(approx_eq(obs_set.variance().unwrap(), 1.6666666667, 1e-9));
        assert!(approx_eq(obs_set.std_dev().unwrap(), 1.290994449, 1e-9));
        assert!(approx_eq(obs_set.std_err().unwrap(), 0.645497224, 1e-9));

        let mut obs_set = GuassianSet::new();
        obs_set.add(1.0);
        obs_set.add(2.0);
        assert!(approx_eq(obs_set.mean().unwrap(), 1.5, 1e-9));
        assert!(approx_eq(obs_set.variance().unwrap(), 0.5, 1e-9));
        assert!(approx_eq(obs_set.std_dev().unwrap(), 0.707106781, 1e-9));
        assert!(approx_eq(obs_set.std_err().unwrap(), 0.5, 1e-9));

        let mut obs_set = GuassianSet::new();
        obs_set.add(-1.0);
        obs_set.add(2.0);
        obs_set.add(3.0);
        obs_set.add(4.0);
        assert!(approx_eq(obs_set.mean().unwrap(), 2.0, 1e-9));
        assert!(approx_eq(obs_set.variance().unwrap(), 4.666666667, 1e-9));
        assert!(approx_eq(obs_set.std_dev().unwrap(), 2.160246899, 1e-9));
        assert!(approx_eq(obs_set.std_err().unwrap(), 1.08012345, 1e-9));
    }
}
