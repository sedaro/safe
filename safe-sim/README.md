# `safe-sim`

`safe-sim` provides Sedaro EDS simulation execution and multi-run study
utilities for SAFE autonomy modes.

The study APIs support two related workflows:

- A **trade study** runs an explicit, ordered set of named cases.
- A **Monte Carlo study** generates cases by sampling independent probability
  distributions with a reproducible seed.

Both workflows retain the case definition and the outcome of every started
simulation. A completed case includes the `SimulationResult` returned by EDS;
launch, timeout, and output-collection failures are recorded without stopping
later cases.

## Requirements

The current implementation is Sedaro-specific. It requires an EDS executable
or an EDS workspace containing an `eds` executable. The registry and simulator
dependencies used by this crate are configured for Sedaro development
environments.

## Trade Studies

Use `TradeStudy` when the cases are known explicitly. A `StudyCase` can carry
numeric parameters for result interpretation and one or more `EdsPatch` values
that are applied only while that case runs.

```rust,no_run
use std::path::PathBuf;

use safe_sim::{EdsPatch, SedaroSimulator, StudyCase, TradeStudy};

# async fn example() -> anyhow::Result<()> {
let simulator = SedaroSimulator::new(&PathBuf::from("path/to/eds"));
let study = TradeStudy::new(simulator, 1.0)
    .case(StudyCase::new("nominal"))
    .case(StudyCase::new("high-mass").patch(EdsPatch::new(
        "agent-id",
        "dynamics",
        "vehicle.mass",
        "f64",
        "110.0",
    )));

let result = study.run().await?;
let high_mass = result.get("high-mass").expect("case exists");
assert!(high_mass.simulation_result().is_some());
# Ok(())
# }
```

Cases run sequentially in insertion order. A non-zero EDS exit is represented
by a completed `SimulationResult` with `success == false`. Failure to launch
EDS, a timeout, or failure while collecting output becomes a
`StudyRunOutcome::Failed`, and later cases still run.

## Monte Carlo Studies

Use `MonteCarloStudy` when uncertain numeric inputs should be sampled before
running the cases. Each parameter has a name, an `EdsPatchTarget`, and a
`ProbabilityDistribution`.

```rust,no_run
use std::path::PathBuf;

use safe_sim::{
    EdsPatchTarget, MonteCarloParameter, MonteCarloStudy, ProbabilityDistribution,
    SedaroSimulator,
};

# async fn example() -> anyhow::Result<()> {
let simulator = SedaroSimulator::new(&PathBuf::from("path/to/eds"));
let mass_target = EdsPatchTarget::new("agent-id", "dynamics", "vehicle.mass", "f64");

let study = MonteCarloStudy::new(simulator, 1.0)
    .samples(100)
    .seed(42)
    .parameter(MonteCarloParameter::new(
        "mass",
        mass_target,
        ProbabilityDistribution::Normal {
            mean: 100.0,
            std_dev: 2.0,
        },
    ));

let result = study.run().await?;
for run in result.successful_runs() {
    let sampled_mass = run.case.parameters["mass"];
    println!("mass={sampled_mass}");
}
# Ok(())
# }
```

The supported distributions are:

- `Normal { mean, std_dev }`
- `Uniform { low, high }`
- `LogNormal { mean, std_dev }`
- `Triangular { low, high, mode }`
- `Discrete { values }`

The top-level seed controls SAFE's sampling. Each generated case also records
its derived case seed. Reusing the same seed, sample count, and ordered
parameter definitions produces the same sampled cases. The seed does not
control random behavior inside EDS.

## Generating Cases Without Running EDS

`generate_cases` is useful when a sampled value needs to be translated into a
larger, mode-specific set of patches. The generated cases contain the sampled
numeric parameters and the patches created from the configured targets.

```rust,no_run
use std::path::PathBuf;

use safe_sim::{
    EdsPatchTarget, MonteCarloParameter, MonteCarloStudy, ProbabilityDistribution,
    SedaroSimulator,
};

# fn example() -> anyhow::Result<()> {
let timing_target = EdsPatchTarget::new(
    "agent-id",
    "power",
    "root!.command_timing_offset_secs",
    "f64",
);
let study = MonteCarloStudy::new(
    SedaroSimulator::new(&PathBuf::from("path/to/eds")),
    1.0,
)
    .samples(10)
    .seed(7)
    .parameter(MonteCarloParameter::new(
        "timing_offset_secs",
        timing_target,
        ProbabilityDistribution::Uniform {
            low: -5.0,
            high: 5.0,
        },
    ));

for case in study.generate_cases()? {
    let offset = case.parameters["timing_offset_secs"];
    // Build additional mode-specific patches from `offset`, then add the case
    // to a TradeStudy if the sampled value represents a larger scenario.
    println!("case={} offset={offset}", case.id);
}
# Ok(())
# }
```

## Results and Cancellation

`StudyResult` preserves the order in which cases were scheduled and provides:

- `get(id)` to find a case by ID
- `successful_runs()` for cases whose EDS process exited successfully
- `failed_runs()` for unsuccessful exits and infrastructure failures
- `remaining_runs()` for cases not started after cancellation

Long-running studies can be cancelled with a shared
`CancellationToken`:

```rust,no_run
use safe_sim::{CancellationToken, TradeStudy};

# async fn example(study: TradeStudy) -> anyhow::Result<()> {
let cancellation = CancellationToken::new();
let task_token = cancellation.clone();
let task = tokio::spawn(async move {
    study.run_with_cancellation(&task_token).await
});

// A lifecycle callback can call this when the mode is deactivated or stopped.
cancellation.cancel();
let result = task.await??;
println!("remaining cases: {}", result.remaining_runs());
# Ok(())
# }
```

Cancellation marks an active case as `Cancelled`; cases that had not started
are not included in `runs`. In an autonomy mode, run a long study in a
background task rather than awaiting it directly from a mode callback, so
deactivation and shutdown can still be processed.

## Execution Constraints

Each `SedaroSimulator` instance uses a separate random EDS results directory.
Cloning a simulator creates another instance with a new results directory.
Caller-supplied `--target-config` arguments are ignored.
Completed results retain all decoded frames in memory, so large studies may need
to be split into smaller batches or have metrics extracted between batches.

By default, the EDS results directory is deleted after collection. Set
`SAFE_DELETE_EDS_RESULTS=false` to retain it for diagnosis.
