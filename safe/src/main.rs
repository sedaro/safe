mod c2;
mod config;
mod definitions;
mod observability;
mod router;
mod transports;
mod kits;

use anyhow::Result;
use async_trait::async_trait;
use c2::{Command, Telemetry};
use config::Config;
use definitions::{
    Activation, Expr, Value, Variable,
};
use figment::providers::{Env, Format, Serialized, Yaml};
use figment::Figment;
use observability as obs;
use router::{AutonomyMode, Router};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::net::{UnixListener, UnixStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt, Interest};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};

use crate::transports::{UnixTransport, Stream};
use crate::transports::Transport;

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
            tx_command
                .send(Command {
                  commanded_attitude: vec![0, 0, 1],
                  thrust: rand::random::<u8>() % 26 + 65,
                })
                .await?;
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
            tx_command
              .send(Command {
                  commanded_attitude: vec![0, 1, 0],
                  thrust: 0,
              })
              .await?;
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

async fn handle_client(mut stream: impl Stream, tx_telemetry: mpsc::Sender<Telemetry>, mut rx_commands: broadcast::Receiver<Command>) {
    loop { // TODO: Figure out how to break out of this loop when client hangs up!
      tokio::select! {
        Ok(msg) = stream.read() => {
          stream.write(msg.clone()).await.ok();
          let tlm: Telemetry = serde_json::from_str(&msg).unwrap();
          println!("Received: {:?}", tlm);
          tx_telemetry.send(tlm).await.ok();
        }
        Ok(cmd) = rx_commands.recv() => {
          let msg = serde_json::to_string(&cmd).unwrap();
          stream.write(msg).await.ok();
        }
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

    let (non_blocking, _guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily("./logs", "safe.log"));
    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_target(false)
        .with_level(true)
        .json()
        .init();

    info!("SAFE is in start up.");

    const SOCKET_PATH: &str = "/tmp/safe.sock";
    // Remove socket if it already exists
    if std::path::Path::new(SOCKET_PATH).exists() {
        fs::remove_file(SOCKET_PATH).await?;
    }
    

    let observability = Arc::new(obs::ObservabilitySubsystem::new(None));
    let observability_clone = observability.clone();
    let config_clone = config.clone();
    tokio::spawn(async move {
        if let Err(e) = observability_clone.run(&config_clone).await {
            warn!("Observability error: {}", e);
        }
    });

    let (tx_telemetry_to_router, rx_telemetry_in_router) =
        mpsc::channel::<Telemetry>(config.router.telem_channel_buffer_size); // TODO: Make channels abstract so we can swap them out for different systems (CPU, MCU, etc.)
    let (tx_command_to_c2, rx_command_in_c2) =
            broadcast::channel(config.router.command_channel_buffer_size);

    // Create router and then register Modes to it.
    // This allows for us to dynamically create and destroy Modes while running.
    let mut router = Router::new(
        rx_telemetry_in_router,
        tx_command_to_c2,
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

    // let c2_listener = TcpListener::bind("127.0.0.1:8001").await?;
    // info!("C2 interface listening on 127.0.0.1:8001");
    // tokio::spawn(async move {
    //     if let Ok((stream, _)) = c2_listener.accept().await {
    //         let transport = Box::new(TcpC2Transport::new(stream));
    //         let _ = c2_interface_task(
    //             transport,
    //             telemetry_tx,
    //             c2_command_rx,
    //             observability,
    //         ).await;
    //     }
    // });

    // let config_listener = TcpListener::bind("127.0.0.1:8002").await?;
    // info!("Config interface listening on 127.0.0.1:8002");
    // tokio::spawn(async move {
    //     if let Ok((stream, _)) = config_listener.accept().await {
    //         let transport = Box::new(TcpConfigTransport::new(stream));
    //         let _ = config_interface_task(transport, config_tx).await;
    //     }
    // });

    tx_telemetry_to_router
        .send(Telemetry {
            timestamp: 12,
            proximity_m: 1200,
        })
        .await?;
    // tx_telemetry_to_router
    //     .send(Telemetry {
    //         timestamp: 13,
    //         proximity_m: 1200,
    //     })
    //     .await?;
    // tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    // tx_telemetry_to_router
    //     .send(Telemetry {
    //         timestamp: 14,
    //         proximity_m: 99,
    //     })
    //     .await?;
    // tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    // tx_telemetry_to_router
    //     .send(Telemetry {
    //         timestamp: 14,
    //         proximity_m: 99,
    //     })
    //     .await?;
    // tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    // tx_telemetry_to_router
    //     .send(Telemetry {
    //         timestamp: 14,
    //         proximity_m: 200,
    //     })
    //     .await?;

    let mut transport = UnixTransport::new(SOCKET_PATH.to_string()).await.unwrap(); // TODO: FIXME
    tokio::spawn(async move {
        loop {
          match transport.accept().await {
            Ok((stream)) => {
                  let tx_telemetry_to_router = tx_telemetry_to_router.clone();
                  let rx_command_in_c2 = rx_command_in_c2.resubscribe();
                  tokio::spawn(async move {
                      handle_client(stream, tx_telemetry_to_router, rx_command_in_c2).await;
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
Other transports
Logs
Config changes
 */

/*
- Have a rust-native autonomy mode or two
- Mode transition command purging
- Try to compile it for Raspberry PI and STM MCU
- Focus on the EDS integration piece
- CLI to issue commands over unix socket to Config and C2 interfaces
- Integrate redb
- Is it important to guarantee that Modes can't issue commands when Router logic would deactivate them?  Do we need to work out the races here or is this acceptable?
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
