mod c2;
mod config;
mod definitions;
mod kits;
mod observability;
mod router;
mod transports;
mod simulation;
mod flight;
mod utils;
mod modes;

use anyhow::Result;
use c2::{TimedCommand, Telemetry};
use definitions::{Activation, Expr, Value, Variable};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::util::SubscriberInitExt;
use tracing::info;
use tracing_subscriber::Layer;
use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;

use crate::modes::contact_analysis::ContactAnalysis;
use crate::modes::multi_objective_optimization::MultiObjectiveOptimization;
use crate::transports::TcpTransport;
use crate::simulation::SedaroSimulator;
use crate::modes::attitude_control_anomaly_recovery::AttitudeControlAnomalyRecovery;
use std::time::Duration;
use crate::flight::Flight;


#[tokio::main]
async fn main() -> Result<()> {

    let mut flight: Flight<Telemetry, TimedCommand> = Flight::new().await
      .client_to_c2_transport(
        TcpTransport::new("127.0.0.1", 8001).await.unwrap_or_else(|e| {
          panic!("Unable to initialized TCP transport: {}", e);
        })
      );

    let mode = AttitudeControlAnomalyRecovery::new(
        "AttitudeControlAnomalyRecovery",
        10,
        Activation::Hysteretic {
            enter: Expr::greater_than(
                Expr::Term(Variable::Float64(Value::TelemetryRef(
                    "pointing_error".to_string(),
                ))),
                Expr::Term(Variable::Float64(Value::Literal(2.0))),
            ),
            exit: Expr::not(Expr::greater_than(
                Expr::Term(Variable::Float64(Value::TelemetryRef(
                  "pointing_error".to_string()
                ))),
                Expr::Term(Variable::Float64(Value::Literal(2.0))),
            )),
        },
        100,
        12,
        SedaroSimulator::new(
          &std::path::PathBuf::from("/Users/sebastianwelsh/Development/sedaro/scf/simulation"),
        ).venv(
          std::path::PathBuf::from("/Users/sebastianwelsh/Development/sedaro/scf/.venv")
        ).timeout(Duration::from_secs_f64(20.0)),
    );
    flight.register_autonomy_mode(mode).await?;
    
    let mode = ContactAnalysis::new(
        "IridiumContactAnalysis",
        50,
        // Activation::Immediate(Expr::not(Expr::Term(Variable::Bool(Value::TelemetryRef("in_sunlight".to_string()))))),
        Activation::Immediate(Expr::Term(Variable::Bool(Value::Literal(true)))),
        SedaroSimulator::new(
          &std::path::PathBuf::from("/Users/sebastianwelsh/Development/sedaro/safe/safe/simulators/iridium"),
        ).venv(
          std::path::PathBuf::from("/Users/sebastianwelsh/Development/sedaro/scf/.venv")
        ).timeout(Duration::from_secs_f64(20.0)),
    );
    flight.register_autonomy_mode(mode).await?;

    let mode = MultiObjectiveOptimization::new(
        "MultiObjectiveConOpsOptimization",
        0,
        Activation::Immediate(Expr::Term(Variable::Bool(Value::Literal(true)))),
        100,
        12,
        SedaroSimulator::new(
          &std::path::PathBuf::from("/Users/sebastianwelsh/Development/sedaro/safe/safe/simulators/imaging"),
        ).venv(
          std::path::PathBuf::from("/Users/sebastianwelsh/Development/sedaro/scf/.venv")
        ).timeout(Duration::from_secs_f64(20.0)),
    );
    flight.register_autonomy_mode(mode).await?;


    /*
    
    MJO
      Needs to continually evaluate future?
      Or lock in a plan and yield back?
      Show statefulness of AMs with gpstime of last command
      Should also save IDs of scheduled commands in case we need to cancel them
      Inputs are:
        - Disk utilization
        - Power system state (battery SoC mainly)
        - OD solution

      Iridium 
      Last contact from ground?
      Successful iridium session - I don't think this exists
      Inputs are 
        - Current pointing plan
        - Time - which drives iridium location
        - OD solution
      
      Collaboration between modes required because pointing plan dictates iridium visibility
      - Collaborate through command schedule?

      Add activation for when telemetry changes value!

      We need to think hard about how schedules and temporary deviations in vehicle config from the model are captured
      e.g. ground turns on a heater for a bit, ad hoc, increasing power consumption.  Need to add this to SAFE or provide SAFE a way to interropgate host for current ConOps
      A full picture of what the future is expected to hold for the vehicle
      
      What is the recommended approach to having two authorities to commanding a system?  Probably to not do it?

      SAFE should keep a buffer of all commands it sent, scheduled, etc and which were accepted so that all modes know what conops looks like ahead
        - Maybe you can initialize or update SAFE with current commands scheduled as well as current state to is knows starting point, beyond just telem
      
      Don't let AM run routines return a result.  They must handle all exceptions themselves?  Or should safe restart them?
      When a channel closes, for whatever reason, implement robust recovery
     */


    // TODO: Move this into flight!!!
    let (non_blocking, _guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily("./logs", "safe.log"));
    let (non_blocking_a, _guard_a) =
      tracing_appender::non_blocking(tracing_appender::rolling::daily("./logs", "AttitudeControlAnomalyRecovery.log"));
    let (non_blocking_b, _guard_b) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily("./logs", "IridiumContactAnalysis.log"));
    let (non_blocking_c, _guard_c) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily("./logs", "MultiObjectiveConOpsOptimization.log"));
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
          .with_filter(EnvFilter::new("[{autonomy_mode=IridiumContactAnalysis}]=trace"))
      )
      .with(
        tracing_subscriber::fmt::layer()
          .with_writer(non_blocking_c)
          .with_target(false)
          .with_level(true)
          .json()
          .with_filter(EnvFilter::new("[{autonomy_mode=MultiObjectiveConOpsOptimization}]=trace"))
      )
      .init();

    flight.run();

    tokio::signal::ctrl_c().await?;
    info!("Shutting down");

    Ok(())
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde::{Deserialize, Serialize};
    use anyhow::Result;
    use crate::{c2::{AutonomyModeMessage, RouterMessage}, definitions::{Activation, Expr, Value, Variable}, flight::Flight, router::AutonomyMode, transports::{Stream, TestTransport, Transport}};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct TestCommand {
        value: String,
    }
    impl TestCommand {
        fn new(value: String) -> Self {
            Self { value, }
        }
    }
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct TestTelemetry {
        value: f64,
    }

    #[derive(Clone, Serialize, Deserialize, Debug)]
    pub struct CommandsRequest {} // FIXME: Centralize with safectl

    #[derive(Debug, Serialize)]
    struct TestAutonomyMode {
        name: String,
        priority: u8,
        activation: Activation,
    }
    impl TestAutonomyMode {
        fn new(name: &str, priority: u8) -> Self {
            Self {
                name: name.to_string(),
                priority,
                activation: Activation::Immediate(Expr::Term(Variable::Bool(
                    Value::Literal(true),
                ))),
            }
        }
        fn with_activation(mut self, activation: Activation) -> Self {
            self.activation = activation;
            self
        }
    }
    #[async_trait]
    impl AutonomyMode<TestTelemetry, TestCommand> for TestAutonomyMode {
        fn name(&self) -> String {
            self.name.clone()
        }
        fn priority(&self) -> u8 {
            self.priority
        }
        fn activation(&self) -> Activation {
            self.activation.clone()
        }
        async fn run(&mut self, mut stream: Box<dyn Stream<AutonomyModeMessage<TestTelemetry>, RouterMessage<TestCommand>>>) -> Result<()> {
            let mut nonce: Option<u64> = None;
            loop {
                // Loop back telem, intentionally ignoring active to test inactive command handling
                if let Ok(message) = stream.read().await {
                  match message {
                    AutonomyModeMessage::Active { nonce: new_nonce } => {
                      nonce = Some(new_nonce);
                    },
                    AutonomyModeMessage::Telemetry(telemetry) => {
                      if nonce.is_some() {
                        stream
                            .write(RouterMessage::Command { 
                              data: TestCommand::new(format!("{} received {}", self.name(), telemetry.value)),
                              nonce: nonce.unwrap(),
                            })
                            .await?;
                      }
                    },
                    AutonomyModeMessage::Inactive => {},
                  }
                }
            }
        }
    }

    #[tokio::test]
    async fn test_flight_basic() {
        let client_transport: TestTransport<String, String> = TestTransport::new(1024);
        let client_transport_handle = client_transport.handle();
        let mut flight = Flight::new().await.client_to_c2_transport(client_transport);

        let mode = TestAutonomyMode::new("Mode A", 0);
        flight.register_autonomy_mode(mode).await.unwrap();

        let mode = TestAutonomyMode::new("Mode B", 1)
          .with_activation(
            Activation::Hysteretic {
                enter: Expr::not(
                    Expr::greater_than(
                      Expr::Term(Variable::Float64(Value::TelemetryRef("value".to_string()))),
                      Expr::Term(Variable::Float64(Value::Literal(100.0))),
                    )
                ),
                exit: Expr::greater_than(
                    Expr::Term(Variable::Float64(Value::TelemetryRef("value".to_string()))),
                    Expr::Term(Variable::Float64(Value::Literal(150.0))),
                ),
            }
        );
        flight.register_autonomy_mode(mode).await.unwrap();
        flight.run();

        let mut client_rx_stream = client_transport_handle.connect().await.unwrap();
        client_rx_stream.write(serde_json::to_string(&CommandsRequest {}).unwrap()).await.unwrap();
        
        let mut client_tx_stream = client_transport_handle.connect().await.unwrap();
        client_tx_stream.write(serde_json::to_string(&TestTelemetry { value: 200.0 }).unwrap()).await.unwrap();
        assert_eq!(serde_json::from_str::<TestCommand>(client_rx_stream.read().await.unwrap().as_str()).unwrap().value, "Mode A received 200".to_string());
        let mut client_tx_stream = client_transport_handle.connect().await.unwrap();
        client_tx_stream.write(serde_json::to_string(&TestTelemetry { value: 99.0 }).unwrap()).await.unwrap();
        assert_eq!(serde_json::from_str::<TestCommand>(client_rx_stream.read().await.unwrap().as_str()).unwrap().value, "Mode B received 99".to_string());
        let mut client_tx_stream = client_transport_handle.connect().await.unwrap();
        client_tx_stream.write(serde_json::to_string(&TestTelemetry { value: 125.0 }).unwrap()).await.unwrap();
        assert_eq!(serde_json::from_str::<TestCommand>(client_rx_stream.read().await.unwrap().as_str()).unwrap().value, "Mode B received 125".to_string());
        let mut client_tx_stream = client_transport_handle.connect().await.unwrap();
        client_tx_stream.write(serde_json::to_string(&TestTelemetry { value: 200.0 }).unwrap()).await.unwrap();
        assert_eq!(serde_json::from_str::<TestCommand>(client_rx_stream.read().await.unwrap().as_str()).unwrap().value, "Mode A received 200".to_string());
    }
}

/*
- IDEA: Way to isolate the exercise/test/develope AMs in isolation without SAFE overhead
- The Transport interface should likely implement a means of ackowledging what has been received
  - This gets more difficult with split streams though
  - Is TCP ack enough?  What about UDP?
- Does the client/server model for transports make sense?  Should router take a stream instead of a transport impls?
  - Document: Passing a handle to the modes allow them to handle reconnect
  - A: It does make sense for flexiblity and the opportunity to flag off certain transports which aren't supported on particular platforms.  Also server-broadcast is nice.  Transports enable platform-agnostic IPC.
- Make unit-testable and more of a framework
- Support background running modes and foreground
  - Implement a way to have background modes which are alerted when they are activated/deactivated
- Documentation, if not sensitive?
  - It is important to guarantee that Modes can't issue commands when Router logic would deactivate them
- Implement autonomy mode transport fault recovery (i.e. reconnect)
  - Unless we come up with somethign more clever, this will require that we implement a handshake to identify the Mode on connect.  This could probably be handle by some connection factory.
- Have a rust-native autonomy mode or two
- (WIP) Utilities for debouncing or filtering out potentially noisy telemetry inputs to get a confident reading.  Make this part of activations for modes.
- Allow for different SAFE instances to "collaborate" via dedicated interface
- Try to compile it for Raspberry PI and STM MCU
- Focus on the EDS integration piece
- Integrate redb
 */

/*
Known issues:
- Need better Activation interpreter
 */

/*
Define ontology
Ontology should be fully persisted in redb
Ontology should support variables which can be updated via config interface
 */

/*
How is routing logic defined?  Part of the Ontology?  Ask team.
Needs to be able to implement arbitrary logic as rust code as a fall back.
 */
