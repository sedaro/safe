use rand::{self, SeedableRng};
use rand_distr::{Distribution, Normal};

pub trait StatisticalDistribution<T> {
    fn sample(&mut self) -> T;
}

#[derive(Debug)]
pub struct NormalDistribution {
    rng: rand::rngs::StdRng,
    distribution: Normal<f64>,
    init_value: f64,
}
impl NormalDistribution {
    pub fn new(mean: f64, std_dev: f64, init_value: f64, seed: usize) -> Self {
        let rng = rand::rngs::StdRng::seed_from_u64(seed as u64);
        let distribution = Normal::new(mean, std_dev).expect("Could not create normal distribution.");
        NormalDistribution { rng, distribution, init_value }
    }
}
impl StatisticalDistribution<f64> for NormalDistribution {
    fn sample(&mut self) -> f64 {
        let sample = self.distribution.sample(&mut self.rng);
        self.init_value + sample
    }
}

mod tests {
    use super::*;

    #[tokio::test]
    async fn test_normal_distribution() {
        let mut dist = NormalDistribution::new(0.0, 0.5, 10.0, 7);
        let initial_sample = dist.sample();
        assert_eq!(initial_sample, 9.478211686635008);
        assert_ne!(initial_sample, dist.sample());

        let mut dist = NormalDistribution::new(0.0, 0.5, 10.0, 7);
        assert_eq!(dist.sample(), initial_sample);

        let mut dist = NormalDistribution::new(0.0, 0.5, 10.0, 8);
        assert_ne!(dist.sample(), initial_sample);
    }
}
