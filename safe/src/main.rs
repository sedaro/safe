mod c2;
mod config;
mod definitions;
mod kits;
mod observability;
mod router;
mod transports;
mod simulation;

use anyhow::Result;
use async_trait::async_trait;
use c2::{Command, Telemetry};
use config::Config;
use definitions::{Activation, Expr, Value, Variable};
use figment::providers::{Env, Format, Serialized, Yaml};
use figment::Figment;
use observability as obs;
use router::{AutonomyMode, Router};
use serde::Serialize;
use simvm::sv::data::FloatValue;
use simvm::sv::ser_de::{dyn_de, dyn_ser};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::util::SubscriberInitExt;
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast, mpsc};
use tracing::{debug, error, info, info_span, warn};
use tokio::sync::Semaphore;
use ordered_float::OrderedFloat;
use tracing_subscriber::Layer;
use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;
use tracing::Instrument;
use time;

use simvm::sv::{combine::TR, data::Datum, parse::Parse, pretty::Pretty, typ::Type};
use crate::transports::Transport;
use crate::transports::TransportHandle;
use crate::transports::{MpscTransport, Stream, TcpTransport, UnixTransport};
use crate::simulation::SedaroSimulator;
use crate::kits::stats::NormalDistribution;
use crate::kits::stats::StatisticalDistribution;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader, AsyncSeekExt, SeekFrom};
use std::time::Duration;

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

#[derive(Debug, Serialize)]
struct GenericUncertaintyQuantificationAutonomyMode {
    name: String,
    priority: u8,
    activation: Option<Activation>,
    N: usize,
    concurrency: usize,
    simulator: SedaroSimulator,
}
#[async_trait]
impl AutonomyMode for GenericUncertaintyQuantificationAutonomyMode {
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
      let semaphore = Arc::new(Semaphore::new(self.concurrency));
      let mut handles = Vec::new();
      let start_time = std::time::Instant::now();
      let success_count = Arc::new(Mutex::new(0.0));
      let fail_count = Arc::new(Mutex::new(0.0));
      let init_file_lock = Arc::new(Mutex::new(()));

      let seed = 7;
      let mut angular_velocity_dist = NormalDistribution::new(0.0, 0.5, 10.0, seed);
      let mut length_dist = NormalDistribution::new(0.0, 0.01, 1.0, seed);

      for i in 0..self.N {
        let permit = semaphore.clone().acquire_owned().await?;
        let simulator = self.simulator.clone();
        
        let angular_velocity = angular_velocity_dist.sample();
        let length = length_dist.sample();

        let success_count_clone = success_count.clone();
        let fail_count_clone = fail_count.clone();
        let init_file_lock_clone = init_file_lock.clone();
        let handle = tokio::spawn(async move { // TODO: Try avoiding the spawn?

          // FIXME: RACE: EDS can start up and end up reading the next EDS runs init file if it gets hung up.
          // - random suffix?
          // - accept file name as input
          let _init_file_guard = init_file_lock_clone.lock().await;
          let init_type = TR::parse("(gnc: (\"root!.angle\": float, \"root!.angleRate\": float, \"root!.elapsedTime\": float, \"root!.length\": float, \"root!.time\": float, \"root!.timeStep\": s),)").unwrap();
          let bytes = std::fs::read("/Users/sebastianwelsh/Development/sedaro/simulation/generated/data/init_5Czl7pn4hDGr6rjwynzGK2f.bin")?; // FIXME
          let init_val = dyn_de(&init_type.typ, &bytes).unwrap();
          let gnc = init_val.get(0).unwrap();
          let mut gnc = gnc.clone();
          gnc.set(1, Datum::Float(FloatValue::F64(OrderedFloat(angular_velocity)))).unwrap();
          gnc.set(3, Datum::Float(FloatValue::F64(OrderedFloat(length)))).unwrap();
          let mut init_val = init_val.clone();
          init_val.set(0, gnc).unwrap(); // FIXME: Ugly
          let bytes = dyn_ser(&init_type.typ, &init_val).unwrap();
          // debug!("Modified simulation input Datum: {:?}", init_val);
          std::fs::write("/Users/sebastianwelsh/Development/sedaro/simulation/generated/data/init_5Czl7pn4hDGr6rjwynzGK2f.bin", bytes)?; // FIXME
          drop(_init_file_guard);

          let result = simulator.run(60.0).await;
          drop(permit); // Release the permit when done
          match &result {
            Ok(output) => {
              match output.status.success() {
                true => *success_count_clone.lock().await += 1.0,
                false => {
                  warn!("Simulation {} failed with non-zero exit code: {:?}", i, String::from_utf8_lossy(&output.stderr));
                  *fail_count_clone.lock().await += 1.0;
                },
              }
            },
            Err(e) => {
              warn!("Simulation failed: {:?}", e);
              *fail_count_clone.lock().await += 1.0;
            },
          }
          result
        }.in_current_span());
        
        handles.push(handle);
        
        // If at 5% increments
        if (i + 1) % (self.N / 20) == 0 {
          let s_count = success_count.clone();
          let s_count = s_count.lock().await.clone();
          let f_count = fail_count.clone();
          let f_count = f_count.lock().await.clone();
          info!(
            "Simulation Rate: {} per second ({} successful, {} failed, {} active)", 
            (s_count + f_count)/start_time.elapsed().as_secs_f64(),
            s_count,
            f_count,
            handles.len() - (s_count as usize) - (f_count as usize),
          );
        }
      }

      // Wait for all simulations to complete
      for handle in handles {
        if let Err(e) = handle.await {
          warn!("Simulation task join error: {:?}", e);
        }
      }
      Ok(())
    }
}

// TODO: Overhaul this handler and the overall Client<>SAFE interface
async fn handle_client(
    mut stream: impl Stream<String, String>,
    mut router_stream: impl Stream<Command, Telemetry>,
) {
    if let Ok(msg) = stream.read().await {
        if let Ok(logs_request) = serde_json::from_str::<c2::LogsRequest>(&msg) { // TODO: Rewrite this once tested and more tightly couple to observability system
            // Tail safe.log.<date> files and send over stream
            let log_dir = std::path::Path::new("./logs");
            let day_date = time::OffsetDateTime::now_utc().date().to_string();
            let file_prefix = match logs_request.mode {
              Some(mode) => mode,
              None => "safe".to_string(),
            };
            let log_file = format!("{}.log.{}", file_prefix, day_date);
            // TODO: Return error response if log stream (i.e. file) doesn't exist
            if let Ok(mut file) = File::open(log_dir.join(log_file)).await {
              // Seek to end of file
              let _ = file.seek(SeekFrom::End(0)).await;
              let mut reader = BufReader::new(file);
              let mut line = String::new();

              loop {
                let curr_day_date = time::OffsetDateTime::now_utc().date().to_string();
                if day_date != curr_day_date {
                  // Day has changed, switch to new log file after sending the rest of current file
                  while let Ok(bytes_read) = reader.read_line(&mut line).await {
                    if bytes_read == 0 {
                      break; // Reached end of file
                    }
                    if let Err(_) = stream.write(line.trim().to_string()).await {
                      return; // Client disconnected
                    }
                    line.clear();
                  }
                  let log_file = format!("{}.log.{}", file_prefix, curr_day_date);
                  if let Ok(new_file) = File::open(log_dir.join(log_file)).await {
                    file = new_file;
                    reader = BufReader::new(file);
                    line.clear();
                  }
                }
                match reader.read_line(&mut line).await {
                  Ok(0) => {
                    // No new data, wait and try again
                    tokio::time::sleep(Duration::from_millis(100)).await;
                  }
                  Ok(_) => {
                    if let Err(_) = stream.write(line.trim().to_string()).await {
                      break; // Client disconnected
                    }
                    line.clear();
                  }
                  Err(_) => break,
                }
              }
            }
        } else if let Ok(tlm) = serde_json::from_str::<c2::Telemetry>(&msg) {
            router_stream.write(tlm).await.ok();
        } else if let Ok(_) = serde_json::from_str::<c2::CommandsRequest>(&msg) {
          loop {
              // TODO: Figure out how to break out of this loop when client hangs up!
              match router_stream.read().await {
                  Ok(cmd) => {
                      let msg = serde_json::to_string(&cmd).unwrap();
                      stream.write(msg).await.ok();
                  }
                  Err(_) => break,
              }
          }
        } else {
            error!("Unknown client message: {}", msg);
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

    // FIXME: Move these to a AM registration step
    let (non_blocking, _guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily("./logs", "safe.log"));
    let (non_blocking_a, _guard_a) =
      tracing_appender::non_blocking(tracing_appender::rolling::daily("./logs", "CollisionAvoidance.log"));
    let (non_blocking_b, _guard_b) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily("./logs", "NominalOps.log"));
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
          .with_filter(EnvFilter::new("[{autonomy_mode=CollisionAvoidance}]=trace"))
      )
      .with(
        tracing_subscriber::fmt::layer()
          .with_writer(non_blocking_b)
          .with_target(false)
          .with_level(true)
          .json()
          .with_filter(EnvFilter::new("[{autonomy_mode=NominalOps}]=trace"))
      )
      .init();

    info!("SAFE is in start up.");

    let observability = Arc::new(obs::ObservabilitySubsystem::new(None));
    let observability_clone = observability.clone();
    let config_clone = config.clone();
    tokio::spawn(async move {
        if let Err(e) = observability_clone.run(&config_clone).await {
            warn!("Observability error: {}", e);
        }
    });

    // let c2_to_router_telemetry_transport: MpscTransport<Telemetry, Command> = MpscTransport::new(config.router.telem_channel_buffer_size);
    let c2_to_router_telemetry_transport: TcpTransport<Telemetry, Command> =
        TcpTransport::new("127.0.0.1", 8000).await?;
    // let c2_to_router_telemetry_transport: UnixTransport<Telemetry, Command> = UnixTransport::new("/tmp/my.sock").await?;
    let handle = c2_to_router_telemetry_transport.handle();

    // Create router and then register Modes to it.
    // This allows for us to dynamically create and destroy Modes while running.
    let mut router = Router::new(
        c2_to_router_telemetry_transport,
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
    let mode = GenericUncertaintyQuantificationAutonomyMode {
        name: "NominalOps".to_string(),
        priority: 0,
        activation: Some(Activation::Immediate(Expr::Term(Variable::Bool(
            Value::Literal(true),
        )))),
        N: 100,
        concurrency: 12,
        simulator: SedaroSimulator::new(
          std::path::PathBuf::from("/Users/sebastianwelsh/Development/sedaro/simulation"),
          "./target/release/main",
          None,
        ),
    };
    router.register_autonomy_mode(mode, &config);

    let config_clone = config.clone();
    tokio::spawn(async move {
        if let Err(e) = router.run(&config_clone).await {
            warn!("Router error: {}", e); // TODO: This needs to be more than a warning
        }
    });

    // let mut transport: UnixTransport<String, String> = UnixTransport::new("/tmp/safe.sock").await?;
    let mut transport: TcpTransport<String, String> = TcpTransport::new("127.0.0.1", 8001).await?;
    tokio::spawn(async move {
        loop {
            match transport.accept().await {
                Ok(stream) => {
                    let c2_telem_stream = handle.connect().await.unwrap();
                    tokio::spawn(async move {
                        handle_client(stream, c2_telem_stream).await;
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
- Add resiliency and reconnect to Transports which can theoretically drop connections (TCP, Unix sockets, etc.)
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
