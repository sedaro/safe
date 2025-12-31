# Sedaro Autonomy Framework for Edge (`safe`)

🚧 `safe` is under active development 🚧

Sedaro Autonomy Framework for Edge (`safe`) is an open-source flight software mission autonomy framework that integrates Sedaro's Edge Deployable Simulators (EDS's) alongside third-party software to achieve trusted satellite autonomy across the mission lifecycle.

![safe](safe.png)

## Yet Another Autonomy Framework?

You're probably already familiar with the Robot Operating System (ROS) which is an existing autonomy framework. Like most autonomy/robotics frameworks, ROS/ROS2 is primarily just a IPC/message-passing implementation with many developer experience features.  Terrestrially, robotics and autonomy teams either use ROS/ROS2 or they roll their own solution.

`safe` is a purpose built capability that is necessarily distinct from ROS (and ROS competitors).  `safe`'s value proposition within this already mature ecosystem stems from three main principles:

1. 🤖 `safe` isn't a real-time robotics framework.  It's an autonomous mission operations framework.  You aren't making your spacecraft a robot by adopting `safe` - hopefully it is already a robot per your fight software (FSW).  Instead, you're making your mission operations a robot.  You're taking what you already do on the ground in the form of scheduling, anomaly resolution, and maintenance and you're deploying it at the edge.  Fully online mission operations, no overpass required.
2. 🧑‍💻 `safe` makes using Sedaro simulators and studies capabilities really easy and fast.  You can typically throw together functional decision making capabilities in under 1 hour with the power of the Sedaro Platform.  Terrestrial mission ops today relies heavily on modeling and simulation by humans.  `safe` takes the ground out of the loop.
3. 📡 `safe` implements host-native C2.  This means that zero new interfaces are required to integrate `safe` onto a host vehicle.  While `safe` is designed to run on board, because `safe` speaks native C2, nothing is stopping you from running `safe` off board in your ops center instead.

## Use Cases

There are two discrete `safe` topologies: Service and Flight Director ("Flight" for short)

### Services
A `Service` can be asked the question "Is this command sequence safe to execute?" and receive back a "yes/no".

### Flight (i.e. Flight Director)

`Flight` is fed host telemetry and produces vetted command sequences to be executed on the host. If desired, commands from a Flight instance can be validated by a Service.

## Architecture

When paired with Sedaro EDS's, `safe` delivers trusted satellite mission autonomy over long durations without ground contact. `safe` realizes a flexible framework that can be readily applied to a diverse array of missions and edge compute devices.  

Features:
1. Autonomy modes offering various levels of intelligence and risk posture interface to core flight software using a built in native C2 interface to consume current system state from telemetry and issue commands to their host vehicle. 
2. A Router sets the active autonomy mode based on telemetry so that developers and mission planners can design an array of modes that incorporate various autonomy approaches for each mission phase, potential state of the vehicle, and potential state of its operating
environment. 
3. Batteries-included developer libraries for multi-simulation, multi-parameter optimization and risk analysis, in addition to turnkey support for the integration of EDS’s, enables streamlined development of `safe` autonomy modes.

## Example

### Flight

```rust
struct Telemetry {
    pointing_error: f64,
}
struct Command {
    set_pid_gains: (f64, f64, f64, f64),
}

struct AttitudeControlRetuneAutonomyMode;
impl AutonomyMode<Telemetry, Command> for AttitudeControlRetuneAutonomyMode {
    fn name(&self) -> String { "Attitude Control Retune".to_string() }
    fn priority(&self) -> u8 { 0 }
    fn activation(&self) -> Activation {
        Activation::Hysteretic {
            enter: Expr::greater_than(
                Expr::Term(Variable::Float64(Value::TelemetryRef("pointing_error".to_string()))),
                Expr::Term(Variable::Float64(Value::Literal(5.0))),
            ),
            exit: Expr::not(
                Expr::greater_than(
                    Expr::Term(Variable::Float64(Value::TelemetryRef("pointing_error".to_string()))),
                    Expr::Term(Variable::Float64(Value::Literal(2.5))),
                )
            ),
        }
    }
    async fn run(&mut self, mut stream: Box<dyn Stream<AutonomyModeMessage<Telemetry>, RouterMessage<Command>>>) -> Result<()> {
        let mut nonce: Option<u64> = None;
        loop {
            if let Ok(message) = stream.read().await {
                match message {
                    AutonomyModeMessage::Active { nonce: new_nonce } => {
                        nonce = Some(new_nonce);
                    },
                    AutonomyModeMessage::Telemetry(telemetry) => {
                        let eds = SedaroSimulator::new(
                            std::path::PathBuf::from("path/to/eds"),
                        ).timeout(Duration::from_secs_f64(5.0));
                        
                        // Update EDS model from latest telemetry
                        init_eds!(eds, telemetry);
                        
                        // Run optimization to find new PID controller gains
                        let gains = optimize1!(eds, "pointing_error", "pid_controller_gains");
                        
                        // Run Monte Carlo of EDS by sampling input distributions for uncertain model parameters
                        let max_speed_obs = GuassianSet::new();
                        let max_pointing_error_obs = GuassianSet::new();
                        init_eds!(eds, pid_controller_gains=gains);
                        uq!(
                            eds,
                            inertia_mat_0_0 = NormalDistribution::new(0.005, 8.33333e-05).seed(1),
                            inertia_mat_1_1 = NormalDistribution::new(0.005, 8.33333e-05).seed(2),
                            inertia_mat_2_2 = NormalDistribution::new(0.005, 8.33333e-05).seed(3),
                            x_wheel_inertia = NormalDistribution::new(0.000005, 8.33333e-08).seed(4),
                            y_wheel_inertia = NormalDistribution::new(0.000005, 8.33333e-08).seed(5),
                            z_wheel_inertia = NormalDistribution::new(0.000005, 8.33333e-08).seed(6),
                            max_speed_obs, 
                            max_pointing_error_obs,
                        );

                        let max_wheel_speed = max_speed_obs.mean() + 3.0 * max_speed_obs.std_dev();
                        let max_pointing_error = max_pointing_error_obs.mean() + 3.0 * max_pointing_error_obs.std_dev();
                        if max_pointing_error < 5.0 && max_wheel_speed < 500.0 {
                          info!("Analysis indicates system meets performance requirements. Proceeding with new controller gains.");
                          stream
                            .write(RouterMessage::Command { 
                              data: Command { set_pid_gains: gains, },
                              nonce: nonce.unwrap(),
                            }).await?;
                        }
                    },
                    AutonomyModeMessage::Inactive => {},
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut flight = Flight::new().await;
    flight.register_autonomy_mode(AttitudeControlRetuneAutonomyMode {}).await.unwrap();
    flight.run();
    tokio::signal::ctrl_c().await?;
    Ok(())
}
```

## Getting Started

```bash
cd ./safe
cargo run
```

For logs, run the following in a different terminal
```bash
cd ./safectl
cargo run -- logs
# or
cargo run -- logs | jq '.fields.message | select( . != null )'
```

For commands, run the following in a different terminal
```bash
cd ./safectl
cargo run -- rx
```

For send telemetry, run the following in a different terminal
```bash
cd ./safectl
cargo run -- tx '{"timestamp": 1625247600, "proximity_m": 200 }'
```


## Project Layout

- [`safe`](./safe/): `safe` implementation
  - [`sedaro`](./safe/sedaro/): Utilities and drivers for integrating [Sedaro](https://sedaro.com) Edge Deployable Simulators (EDS)
- [`safectl`](./safectl/): A CLI for interacting with a running `safe` process
- [`examples`](./examples/): Example autonomy implementations and reference designs


## License

This project is licensed under the [Apache-2.0 License](./LICENSE).