use anyhow::Result;
use async_trait::async_trait;
use base64::prelude::BASE64_STANDARD;
use figment::providers::{Env, Format, Serialized, Yaml};
use figment::Figment;
use serde::{Deserialize, Serialize};
use simvm::sv::data::{Data, FloatValue};
use simvm::sv::ser_de::{dyn_de, dyn_ser};
use tracing::instrument::WithSubscriber;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::fmt::format::{Json, JsonFields};
use tracing_subscriber::layer::Layered;
use tracing_subscriber::{EnvFilter, Registry};
use tracing_subscriber::util::SubscriberInitExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::vec;
use tracing::{error, info, warn};
use tokio::sync::{Mutex, Semaphore};
use ordered_float::OrderedFloat;
use tracing_subscriber::Layer;
use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;
use tracing::Instrument;
use time;
use base64::Engine;

use crate::c2::{Command, Telemetry};
use crate::config::Config;
use crate::definitions::{Activation, Expr, Value, Variable};
use crate::observability as obs;
use crate::router::{AutonomyMode, Router};
use simvm::sv::{combine::TR, data::Datum, parse::Parse, pretty::Pretty};
use crate::c2::{CommandsRequest, LogsRequest};
use crate::transports::{MpscTransportHandle, Transport};
use crate::transports::TransportHandle;
use crate::transports::{MpscTransport, Stream, TcpTransport, UnixTransport};
use crate::simulation::SedaroSimulator;
use crate::kits::stats::{GuassianSet, NormalDistribution};
use crate::kits::stats::StatisticalDistribution;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader, AsyncSeekExt, SeekFrom};
use std::time::Duration;

// Cute form of "Flight Director"
// TODO: Consider alternative name later

pub struct Build;
pub struct Run;

pub struct Flight<T, C, State = Build> {
  _state: std::marker::PhantomData<State>,
  config: Config,
  router: Router<T, C>,
  // router: Arc<Mutex<Router<MpscTransport<Telemetry, Command>>>>, // TODO: Make generic
  tracing_layers: Vec<tracing_subscriber::fmt::Layer<Registry, JsonFields, tracing_subscriber::fmt::format::Format<Json>, NonBlocking>>,
  // tracing_layers: Vec<tracing_subscriber::fmt::Layer<Registry>>,
  tracing_guards: HashMap<String, WorkerGuard>,
  c2_to_router_transport_handle: Box<dyn TransportHandle<T, C>>,
  client_to_c2_transport: Option<Box<dyn Transport<String, String>>>,
  // client_to_c2_transport: Option<Arc<Mutex<TcpTransport<String, String>>>>, // TODO: Make generic
}


impl<T, C> Flight<T, C, Build>
where
  T: Clone + Serialize + for<'de>Deserialize<'de> + Send + 'static + Sync,
  C: Clone + Serialize + for<'de>Deserialize<'de> + Send + 'static + Sync,
{
    pub async fn new() -> Self {

        let config = Figment::new()
          // Start with defaults
          .merge(Serialized::defaults(Config::default()))
          // Load from config file (optional)
          .merge(Yaml::file("safe.yaml").nested())
          // Override with environment variables
          // Format: SAFE__ROUTER__MAX_AUTONOMY_MODES=128
          .merge(Env::prefixed("SAFE__").split("__"))
          .extract::<Config>()
          .unwrap_or_else(|e| {
              panic!("Failed to load user-defined configuration: {}.  Continuing.", e);
          });

        let (non_blocking, _guard) =
            tracing_appender::non_blocking(tracing_appender::rolling::daily("./logs", "safe.log"));
        let root_tracing_layer = tracing_subscriber::fmt::layer()
          .with_writer(non_blocking)
          .with_target(false)
          .with_level(true)
          .json();

        info!("SAFE is in start up.");
        println!("SAFE is in start up.");

        let observability = Arc::new(obs::ObservabilitySubsystem::new(None));
        let observability_clone = observability.clone();
        let config_clone = config.clone();
        tokio::spawn(async move {
            if let Err(e) = observability_clone.run(&config_clone).await {
                warn!("Observability error: {}", e);
            }
        });

        let c2_to_router_transport: MpscTransport<T, C> = MpscTransport::new(config.router.telem_channel_buffer_size);
        // let c2_to_router_transport: TcpTransport<Telemetry, Command> = TcpTransport::new("127.0.0.1", 8000).await?;
        // let c2_to_router_transport = UnixTransport::new("/tmp/safe.sock").await.unwrap_or_else(|e| {
        //   panic!("Unable to initialize unix transport: {}", e);
        // });
        let c2_to_router_transport_handle = c2_to_router_transport.handle();
        let router = Router::new( // TODO: Consider just making this part of Flight
            Box::new(c2_to_router_transport),
            observability.clone(),
            &config,
        );

        // let mut transport: UnixTransport<String, String> = UnixTransport::new("/tmp/safe.sock").await?;
        // let mut client_to_c2_transport: TcpTransport<String, String> = TcpTransport::new("127.0.0.1", 8001).await.unwrap_or_else(|e| {
        //   panic!("Unable to initialized TCP transport: {}", e);
        // });
        
        Flight {
          _state: std::marker::PhantomData,
          config,
          router,
          tracing_layers: vec![root_tracing_layer],
          tracing_guards: HashMap::new(),
          c2_to_router_transport_handle,
          client_to_c2_transport: None,
        }
    }
    pub fn c2_to_router_transport(mut self, transport: impl Transport<T, C> + 'static) -> Self {
      self.c2_to_router_transport_handle = transport.handle();
      self.router = self.router.c2_transport(transport);
      self
    }
    pub fn client_to_c2_transport(mut self, transport: impl Transport<String, String> + 'static) -> Self {
      self.client_to_c2_transport = Some(Box::new(transport));
      self
    }
    // TODO: Build out rest of configurability here as continuous of builder pattern rather than constructor params

    pub fn run(mut self) -> () {
      // Start client handler (if transport is set)
      if let Some(mut client_transport) = self.client_to_c2_transport {
        let handle = self.c2_to_router_transport_handle;
        println!("SAFE C2 listening on {}", client_transport);
        tokio::spawn(async move {
            loop {
                match (*client_transport).accept().await {
                    Ok(stream) => {
                        let c2_telem_stream = handle.connect().await.unwrap();
                        tokio::spawn(async move {
                            Flight::handle_c2_connection(stream, c2_telem_stream).await;
                        });
                    }
                    Err(e) => eprintln!("Connection error: {}", e),
                }
            }
        });
      }

      // Start Router
      let config_clone = self.config.clone();
      tokio::spawn(async move {
          if let Err(e) = self.router.run(&config_clone).await {
              warn!("Router error: {}", e); // TODO: This needs to be more than a warning
          }
      });
    }
}

impl <T, C> Flight<T, C>
where
  T: Clone + Serialize + for<'de>Deserialize<'de> + Send + 'static + Sync,
  C: Clone + Serialize + for<'de>Deserialize<'de> + Send + 'static + Sync,
{
    // TODO: Support registering and deregistering modes while running
    pub async fn register_autonomy_mode<M: AutonomyMode<T, C>>(&mut self, mode: M) -> Result<()> { // TODO: Implement dynamic registration
      let (non_blocking, guard) =
          tracing_appender::non_blocking(tracing_appender::rolling::daily("./logs", format!("{}.log", mode.name().clone())));
      // let tracing_registry = &self.tracing_registry;
      self.tracing_guards.insert(mode.name(), guard);

      tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_target(false)
        .with_level(true)
        .json()
        .with_filter(EnvFilter::new(format!("[{{autonomy_mode={}}}]=trace", mode.name().clone())))
      );
      self.router.register_autonomy_mode(mode).await?;
      Ok(())
    }
    pub fn unregister_autonomy_mode(&mut self, name: &str) {
      self.tracing_guards.remove(name);
      unimplemented!();
    }
}

impl <T, C> Flight<T, C, Run>
where
  T: Clone + Serialize + for<'de>Deserialize<'de> + Send + 'static + Sync,
  C: Clone + Serialize + for<'de>Deserialize<'de> + Send + 'static + Sync,
{ 
    // TODO: Overhaul this handler and the overall Client<>SAFE interface
    async fn handle_c2_connection(
        mut stream: Box<dyn Stream<String, String>>,
        mut router_stream: Box<dyn Stream<C, T>>,
    ) {
        if let Ok(msg) = stream.read().await {
            if let Ok(logs_request) = serde_json::from_str::<LogsRequest>(&msg) { // TODO: Rewrite this once tested and more tightly couple to observability system
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
            } else if let Ok(tlm) = serde_json::from_str::<T>(&msg) {
                router_stream.write(tlm).await.ok();
            } else if let Ok(_) = serde_json::from_str::<CommandsRequest>(&msg) {
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
}