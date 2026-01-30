use crate::simulation::{SedaroSimulator, SimulationResult};
use tokio::time::Duration;

/// Status of the optimization process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OptimizationStatus {
    Converged,
    MaxIterationsExceeded,
    Timeout,
    UnsupportedProblem(String),
    Error(String),
}

/// Result of an optimization process.
#[derive(Clone, Debug)]
pub struct OptimizationResult<T> {
    /// The final status of the optimization process.
    pub status: OptimizationStatus,

    /// The performance metric achieved by the best solution.
    pub performance: f64,

    /// The best solution found during optimization.
    pub best: T,

    /// The total runtime of the optimization process in milliseconds.
    pub runtime: f64,

    /// The number of simulation runs performed during optimization.
    pub calls: u64,
}

/// Configuration for optimization routines.
#[derive(Clone, Debug)]
pub struct Problem<T> {
    /// The simulator instance to be used for optimization.
    simulator: SedaroSimulator,

    /// The objective function to evaluate the performance of a simulation result.
    objective: fn(&SimulationResult) -> f64,

    /// Constraints that must be satisfied during optimization.
    constraints: Vec<fn(&SimulationResult) -> bool>,

    /// Initial state or parameters for the optimization process.
    initial_state: T,

    /// Maximum allowed total runtime for the optimization process in milliseconds.
    timeout_ms: u64,
}

impl<T> Problem<T> {
    pub fn new(
        simulator: SedaroSimulator,
        objective: fn(&SimulationResult) -> f64,
        constraints: Vec<fn(&SimulationResult) -> bool>,
        initial_state: T,
        timeout_ms: u64,
    ) -> Self {
        Problem {
            simulator,
            objective,
            constraints,
            initial_state,
            timeout_ms,
        }
    }

    pub fn evaluate(&self, value: &T) -> SimulationResult {
        // FIXME: implement SedaroSimulator -> SimulationResult
        // (self.objective)(&self.simulator.run_simulation(value))
        SimulationResult {}
    }
}

/// Trait defining the interface for optimization algorithms.
///
/// This is the primary interface for implementing optimization routines.
pub trait Optimizer<T> {
    async fn optimize(&self, problem: &Problem<T>) -> OptimizationResult<T>;

    fn initial_objective(&self, problem: &Problem<T>) -> f64 {
        let initial_result = problem.evaluate(&problem.initial_state);
        (problem.objective)(&initial_result)
    }
}

#[cfg(test)]
mod tests {
    use super::{OptimizationResult, OptimizationStatus, Optimizer, Problem};
    use crate::simulation::{SedaroSimulator, SimulationResult};
    use tokio;

    fn simple_objective(_result: &SimulationResult) -> f64 {
        0.0 // Placeholder objective function
    }

    struct SimpleOptimizer {}

    impl Optimizer<f64> for SimpleOptimizer {
        async fn optimize(&self, problem: &Problem<f64>) -> OptimizationResult<f64> {
            // A dummy optimizer that just returns the initial state as the best solution.
            OptimizationResult {
                status: OptimizationStatus::Converged,
                performance: simple_objective(&problem.evaluate(&problem.initial_state)),
                best: problem.initial_state,
                runtime: 0.0,
                calls: 1,
            }
        }
    }

    #[tokio::test]
    async fn test_simple_optimization() {
        let problem = Problem {
            simulator: SedaroSimulator::new("/tmp/path".into()),
            objective: |_result: &SimulationResult| 0.0,
            constraints: vec![],
            initial_state: 42.0,
            timeout_ms: 1000,
        };
        let optimizer = SimpleOptimizer {};

        let initial_objective = optimizer.initial_objective(&problem);
        assert_eq!(initial_objective, 0.0);

        let result = optimizer.optimize(&problem).await;
        assert_eq!(result.status, OptimizationStatus::Converged);
        assert_eq!(result.best, 42.0);
    }
}
