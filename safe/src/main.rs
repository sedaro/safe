// main.rs
use anyhow::Result;
use async_trait::async_trait;
use figment::Figment;
use figment::providers::{Env, Format, Serialized, Yaml};
use rand_distr::num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio::time;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::{info, warn, debug};
use futures::{SinkExt, StreamExt};
use sysinfo::{System, Pid};
use std::process;

// ============================================================================
// Message Types
// ============================================================================

#[derive(Clone, Serialize, Deserialize, Debug)]
struct Telemetry {
    timestamp: u64,
    altitude_m: i32,
    velocity_mps: i16,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
struct Command {
    cmd_id: u16,
    payload: Vec<u8>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
enum ConfigMessage {
    SetAction(EngagementMode),
    AddMode { name: String, config: String },
    RemoveMode { name: String },
    QueryTelemetry,
    QueryLogs { start: u64, end: u64 },
}

#[derive(Clone, Serialize, Deserialize, Debug)]
enum EngagementMode {
    Off,
    Passive,
    Active,
}

enum AutonomyModeSignal {
  // TODO: Warn of getting unscheduled
}

#[derive(Clone, Serialize, Deserialize, Debug)]
struct AuditEntry {
    sequence: u64,
    timestamp: u64,
    event_type: String,
    payload: Vec<u8>,
    reason: Option<String>, // TODO: Consider renaming to `explanation`
}

// ============================================================================
// C2 Transport Abstraction
// ============================================================================

#[async_trait]
trait C2Transport: Send + Sync {
    async fn recv_telemetry(&mut self) -> Result<Telemetry>;
    async fn send_command(&mut self, cmd: Command) -> Result<()>;
}

struct TcpC2Transport {
    framed: Framed<TcpStream, LengthDelimitedCodec>,
}

impl TcpC2Transport {
    fn new(stream: TcpStream) -> Self {
        Self {
            framed: Framed::new(stream, LengthDelimitedCodec::new()),
        }
    }
}

#[async_trait]
impl C2Transport for TcpC2Transport {
    async fn recv_telemetry(&mut self) -> Result<Telemetry> {
        let bytes = self.framed.next().await
            .ok_or_else(|| anyhow::anyhow!("Connection closed"))??;
        Ok(bincode::deserialize(&bytes)?)
    }

    async fn send_command(&mut self, cmd: Command) -> Result<()> {
        let bytes = bincode::serialize(&cmd)?;
        self.framed.send(bytes.into()).await?;
        Ok(())
    }
}

// ============================================================================
// Config Transport
// ============================================================================

#[async_trait]
trait ConfigTransport: Send + Sync {
    async fn recv_config(&mut self) -> Result<ConfigMessage>;
    async fn send_response(&mut self, response: String) -> Result<()>;
}

struct TcpConfigTransport {
    framed: Framed<TcpStream, LengthDelimitedCodec>,
}

impl TcpConfigTransport {
    fn new(stream: TcpStream) -> Self {
        Self {
            framed: Framed::new(stream, LengthDelimitedCodec::new()),
        }
    }
}

#[async_trait]
impl ConfigTransport for TcpConfigTransport {
    async fn recv_config(&mut self) -> Result<ConfigMessage> {
        let bytes = self.framed.next().await
            .ok_or_else(|| anyhow::anyhow!("Connection closed"))??;
        Ok(bincode::deserialize(&bytes)?)
    }

    async fn send_response(&mut self, response: String) -> Result<()> {
        let bytes = response.into_bytes();
        self.framed.send(bytes.into()).await?;
        Ok(())
    }
}

// ============================================================================
// Observability System
// ============================================================================

struct ObservabilitySubsystem {
    seq: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
enum LogEntry {
    Generic(String),
    Metrics { uptime: u64, memory: f64, disk_read: f64, disk_write: f64, cpu: f32 },
    CommandIssued { commands: Vec<Command>, reason: Option<String> },
    TelemetryReceived(Telemetry),
    // ConfigChanged { before: , after },
}

impl ObservabilitySubsystem {
    fn new(seq: Option<u64>) -> Self {
        Self {
            seq: Arc::new(std::sync::atomic::AtomicU64::new(seq.unwrap_or(0))),
        }
    }

    // TODO: Rework this
    // We want to only capture replayable events in this log and name it accordingly.  The file should only have events in it
    // There should be a different log for generic message
    fn write(&self, level: tracing::Level, entry: LogEntry) {
        let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        match level {
            tracing::Level::ERROR => tracing::error!(seq = seq, timestamp = timestamp, entry = ?entry),
            tracing::Level::WARN => tracing::warn!(seq = seq, timestamp = timestamp, entry = ?entry),
            tracing::Level::INFO => tracing::info!(seq = seq, timestamp = timestamp, entry = ?entry),
            tracing::Level::DEBUG => tracing::debug!(seq = seq, timestamp = timestamp, entry = ?entry),
            tracing::Level::TRACE => tracing::trace!(seq = seq, timestamp = timestamp, entry = ?entry),
        }
    }

    async fn run(&self, config: &Config) -> Result<()> {
        let mut sys = System::new_all();
        let pid = Pid::from(process::id() as usize);
        let mut interval = time::interval(std::time::Duration::from_secs(config.observability.metrics.period_seconds));
        interval.tick().await; 
        loop {
          sys.refresh_process(pid);
          let Some(process) = sys.process(pid) else {
            warn!("Failed to get process info for metrics");
            continue;
          };
          self.write( // TODO: Validate and include entries for each autonomy mode (and all other parallel processes).  This will also not account for anything running outside of the process that is connected in via IPC.  Modes will need their own way or reporting possibly.
            tracing::Level::INFO,
            LogEntry::Metrics { 
              uptime: process.run_time(), // seconds
              memory: process.memory().to_f64().unwrap()/1024.0/1024.0, // MB
              disk_read: process.disk_usage().read_bytes.to_f64().unwrap()/1024.0/1024.0, // MB
              disk_write: process.disk_usage().written_bytes.to_f64().unwrap()/1024.0/1024.0, // MB
              cpu: process.cpu_usage(), // percentage
            },
          );
          interval.tick().await;
        }
    }
}

// ============================================================================
// Autonomy Mode
// ============================================================================

#[derive(Debug)]
struct AutonomyMode {
  name: String,
  rx_telem: broadcast::Receiver<Telemetry>,
  tx_command: mpsc::Sender<Command>,
  active: bool,
  priority: u8,
  activation: Option<Activation>,
}
impl AutonomyMode {
  async fn run(&mut self) -> Result<()> {
      loop {
          if let Ok(telemetry) = self.rx_telem.recv().await {
            info!("{} received telemetry: {:?}", self.name, telemetry);
            self.tx_command.send(Command { cmd_id: 1, payload: vec![0, 1, 2] }).await?;
          }
      }
  }
}

// TODO: Get feedback from Team on all of this Ontology!
// TODO: SedaroTS here instead?

#[derive(Serialize, Deserialize, Debug, Clone)]
struct AutonomyModeDefinition {
  pub name: String,
  pub priority: u8,
  pub activation: Option<Activation>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct VariableDefinition<T> {
  pub name: String,
  pub initial_value: Option<T>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
enum GenericVariable<T> {
  Literal(T),
  VariableRef(String),
  TelemetryRef(String),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
enum Variable {
  String(GenericVariable<String>),
  Float64(GenericVariable<f64>),
  Bool(GenericVariable<bool>),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
enum Expr {
  Variable,
  And(Vec<Expr>),
  Or(Vec<Expr>),
  Not(Box<Expr>),
  GreaterThan(Box<Variable>, Box<Variable>),
  LessThan(Box<Variable>, Box<Variable>),
  Equal(Box<Variable>, Box<Variable>),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
enum Activation {
  Immediate(Expr),
  Hysteretic { enter: Expr, exit: Expr },
}

// ============================================================================
// Router
// ============================================================================

struct Router {
    engagement_mode: EngagementMode,
    rx_telem: mpsc::Receiver<Telemetry>,
    tx_telem_to_modes: broadcast::Sender<Telemetry>,
    rx_telem_in_modes: broadcast::Receiver<Telemetry>,
    tx_command: mpsc::Sender<Command>,
    observability: Arc<ObservabilitySubsystem>,
    selected_mode: Option<String>,
    autonomy_modes: HashMap<String, (Arc<tokio::sync::Mutex<AutonomyMode>>, mpsc::Receiver<Command>)>,
}

impl Router {
    fn new(
        rx_telem: mpsc::Receiver<Telemetry>,
        tx_command: mpsc::Sender<Command>,
        observability: Arc<ObservabilitySubsystem>,
        config: &Config,
    ) -> Self {
        debug!("Initializing Router with config: {:?}", config.router);
        let (tx_telem_to_modes, rx_telem_in_modes) = broadcast::channel(config.router.max_autonomy_modes);
        Self {
            engagement_mode: EngagementMode::Off,
            rx_telem,
            tx_telem_to_modes,
            rx_telem_in_modes,
            tx_command,
            observability,
            selected_mode: None,
            autonomy_modes: HashMap::new(),
        }
    }

    fn register_autonomy_mode(&mut self, mode_def: &AutonomyModeDefinition, config: &Config) {
        let (tx_command_to_router, rx_command_from_modes) = mpsc::channel::<Command>(config.router.command_channel_buffer_size);
        let mode = AutonomyMode {
          name: mode_def.name.clone(),
          rx_telem: self.rx_telem_in_modes.resubscribe(),
          tx_command: tx_command_to_router.clone(),
          active: false,
          priority: mode_def.priority,
          activation: mode_def.activation.clone(),
        };
        let mode = Arc::new(tokio::sync::Mutex::new(mode));
        let mode_for_task = mode.clone();
        tokio::spawn(async move { // TODO: Make thread
          let mut mode = mode_for_task.lock().await;
          if let Err(e) = mode.run().await {
            warn!("Autonomy Mode error: {}", e);
          }
        });
        self.autonomy_modes.insert(mode_def.name.to_string(), (mode, rx_command_from_modes));
        self.selected_mode = Some(mode_def.name.to_string()); // TODO: Remove and based on router activations
    }

    async fn run(&mut self) -> Result<()> {
        info!("Router is starting");
        loop {
            tokio::select! {
                Some(telemetry) = self.rx_telem.recv() => {
                    self.observability.write(
                        tracing::Level::INFO,
                        LogEntry::TelemetryReceived(telemetry.clone()),
                    );
                    if let Err(_) = self.tx_telem_to_modes.send(telemetry) {
                        warn!("No active subscribers for telemetry");
                    }
                }
                // TODO: There is an issue here where modes can send a bunch of commands when inactive and then when the become active we'll forward them along
                // Need to flush the channels when switching modes
                Some(command) = self.autonomy_modes.get_mut(self.selected_mode.as_ref().unwrap()).unwrap().1.recv() => {
                    self.observability.write(
                        tracing::Level::INFO,
                        LogEntry::CommandIssued { // TODO: Have this include which mode issued it
                            commands: vec![command.clone()],
                            reason: None,
                        },
                    );
                    if let Err(_) = self.tx_command.send(command).await {
                        warn!("No active subscribers for commands");
                    }
                }
                // Some(config) = self.config_rx.recv() => {
                //     self.handle_config(config).await?;
                // }
            }
        }
    }
}

// ============================================================================
// C2 Interface Task
// ============================================================================

async fn c2_interface_task(
    mut transport: Box<dyn C2Transport>,
    telem_tx: mpsc::Sender<Telemetry>,
    mut command_rx: mpsc::Receiver<Command>,
    observability: Arc<ObservabilitySubsystem>,
) -> Result<()> {
    info!("C2 interface started");
    
    loop {
        tokio::select! {
            result = transport.recv_telemetry() => {
                match result {
                    Ok(telemetry) => {
                        observability.write(
                            tracing::Level::INFO,
                            LogEntry::TelemetryReceived(telemetry.clone()),
                        );
                        telem_tx.send(telemetry).await?;
                    }
                    Err(e) => {
                        warn!("C2 recv error: {}", e);
                        break;
                    }
                }
            }
            Some(cmd) = command_rx.recv() => {
                transport.send_command(cmd).await?;
            }
        }
    }
    
    Ok(())
}

// ============================================================================
// Config Interface Task
// ============================================================================

async fn config_interface_task(
    mut transport: Box<dyn ConfigTransport>,
    config_tx: mpsc::Sender<ConfigMessage>,
) -> Result<()> {
    info!("Config interface started");
    
    loop {
        match transport.recv_config().await {
            Ok(config) => {
                config_tx.send(config).await?;
                transport.send_response("OK".to_string()).await?;
            }
            Err(e) => {
                warn!("Config recv error: {}", e);
                break;
            }
        }
    }
    
    Ok(())
}

// ============================================================================
// Configuration Structures
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub observability: ObservabilityConfig,
    pub router: RouterConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct C2Config {
    pub transport: String,  // "tcp", "udp", "serial", "mock"
    pub address: String,
    pub port: u16,
    pub timeout_ms: u64,
    pub reconnect_interval_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    pub log_level: String, // TODO: Hookup
    pub rotation: String,  // TODO: Hookup
    pub metrics: MetricsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub period_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterConfig {
    pub max_autonomy_modes: usize,
    pub telem_channel_buffer_size: usize,
    pub command_channel_buffer_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // c2: C2Config {
            //     transport: "tcp".to_string(),
            //     address: "127.0.0.1".to_string(),
            //     port: 8001,
            //     timeout_ms: 5000,
            //     reconnect_interval_ms: 1000,
            // },
            observability: ObservabilityConfig {
                log_level: "info".to_string(),
                rotation: "daily".to_string(),
                metrics: MetricsConfig {
                    enabled: true,
                    period_seconds: 10,
                }
            },
            router: RouterConfig {
                max_autonomy_modes: 64,
                telem_channel_buffer_size: 1000,
                command_channel_buffer_size: 100,
            },
        }
    }
}

// ============================================================================
// Main
// ============================================================================

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
    
    let (non_blocking, _guard) = tracing_appender::non_blocking(
        tracing_appender::rolling::daily("./logs", "safe.log")
    );
    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_target(false)
        .with_level(true)
        .json()
        .init();
    
    let thing = AutonomyModeDefinition {
      name: "CollisionAvoidance".to_string(),
      priority: 1,
      activation: Some(Activation::Hysteretic {
        enter: Expr::GreaterThan(
          Box::new(Variable::Float64(GenericVariable::TelemetryRef("proximity_m".to_string()))),
          Box::new(Variable::Float64(GenericVariable::Literal(100.0))),
        ),
        exit: Expr::LessThan(
          Box::new(Variable::Float64(GenericVariable::TelemetryRef("proximity_m".to_string()))),
          Box::new(Variable::Float64(GenericVariable::Literal(150.0))),
        ),
      }),
    };
    let var = VariableDefinition::<f64> {
      name: "proximity_m".to_string(),
      initial_value: Some(0.0),
    };
    let v = serde_json::to_string_pretty(&thing)?;
    println!("{}", v);
    let v = serde_json::from_str::<AutonomyModeDefinition>(&v)?;
    println!("{:?}", v);
    let v = serde_json::to_string_pretty(&var)?;
    println!("{}", v);
    let v = serde_json::from_str::<VariableDefinition<f64>>(&v)?;
    println!("{:?}", v);

    info!("SAFE is in start up.");
    
    let observability = Arc::new(ObservabilitySubsystem::new(None));
    let observability_clone = observability.clone();
    let config_clone = config.clone();
    tokio::spawn(async move {
        if let Err(e) = observability_clone.run(&config_clone).await {
            warn!("Observability error: {}", e);
        }
    });

    let (tx_telemetry_to_router, rx_telemetry_in_router) = mpsc::channel::<Telemetry>(config.router.telem_channel_buffer_size); // TODO: Make channels abstract so we can swap them out for different systems (CPU, MCU, etc.)
    let (tx_command_to_c2, rx_command_in_c2) = mpsc::channel::<Command>(config.router.command_channel_buffer_size);
    
    let mut router = Router::new(
        rx_telemetry_in_router,
        tx_command_to_c2,
        observability.clone(),
        &config,
    );

    let mode_def = AutonomyModeDefinition {
      name: "CollisionAvoidance".to_string(),
      priority: 1,
      activation: Some(Activation::Hysteretic {
        enter: Expr::GreaterThan(
          Box::new(Variable::Float64(GenericVariable::TelemetryRef("proximity_m".to_string()))),
          Box::new(Variable::Float64(GenericVariable::Literal(100.0))),
        ),
        exit: Expr::LessThan(
          Box::new(Variable::Float64(GenericVariable::TelemetryRef("proximity_m".to_string()))),
          Box::new(Variable::Float64(GenericVariable::Literal(150.0))),
        ),
      }),
    };
    router.register_autonomy_mode(&mode_def, &config);

    let mode_def = AutonomyModeDefinition {
      name: "Other".to_string(),
      priority: 1,
      activation: Some(Activation::Hysteretic {
        enter: Expr::GreaterThan(
          Box::new(Variable::Float64(GenericVariable::TelemetryRef("proximity_m".to_string()))),
          Box::new(Variable::Float64(GenericVariable::Literal(100.0))),
        ),
        exit: Expr::LessThan(
          Box::new(Variable::Float64(GenericVariable::TelemetryRef("proximity_m".to_string()))),
          Box::new(Variable::Float64(GenericVariable::Literal(150.0))),
        ),
      }),
    };
    router.register_autonomy_mode(&mode_def, &config);
    
    tokio::spawn(async move {
        if let Err(e) = router.run().await {
            warn!("Router error: {}", e);
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

    tx_telemetry_to_router.send(Telemetry { timestamp: 12, altitude_m: 12, velocity_mps: 12 }).await?;
    tx_telemetry_to_router.send(Telemetry { timestamp: 13, altitude_m: 12, velocity_mps: 12 }).await?;
    
    tokio::signal::ctrl_c().await?;
    info!("Shutting down");
    
    Ok(())
}

/*
AutonomyMode should be converted back to a trait so we can actually do stuff in rust natively?  We want to run it as its 
own thread though so what's the suggestion for managing Routing state while also having independence.
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
 */

/*
Create router and then register Modes to it.
This allows for us to dynamically create and destroy modes as needed.
 */