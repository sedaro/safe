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
use std::env;
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
          &std::path::PathBuf::from("./simulators/imaging"),
        ).venv(
          std::path::PathBuf::from(env::var("SAFE_VENV").unwrap())
        ).timeout(Duration::from_secs_f64(20.0)),
    );
    flight.register_autonomy_mode(mode).await?;
    
    let mode = ContactAnalysis::new(
        "IridiumContactAnalysis",
        5,
        Activation::Immediate(Expr::not(Expr::Term(Variable::Bool(Value::TelemetryRef("in_sunlight".to_string()))))),
        // Activation::Immediate(Expr::Term(Variable::Bool(Value::Literal(true)))),
        SedaroSimulator::new(
          &std::path::PathBuf::from("./simulators/iridium"),
        ).venv(
          std::path::PathBuf::from(env::var("SAFE_VENV").unwrap())
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
          &std::path::PathBuf::from("./simulators/imaging"),
        ).venv(
          std::path::PathBuf::from(env::var("SAFE_VENV").unwrap())
        ).timeout(Duration::from_secs_f64(20.0)),
    );
    flight.register_autonomy_mode(mode).await?;

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
