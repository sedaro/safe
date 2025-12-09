use crate::c2::{Command, Telemetry};
use crate::config::{Config, EngagementMode};
use crate::definitions::Variable;
use crate::definitions::{Activation, Resolvable, Value};
use crate::observability as obs;
use crate::transports::ReadHalf;
use crate::transports::Stream;
use crate::transports::Transport;
use crate::transports::WriteHalf;
use anyhow::Result;
use async_trait::async_trait;
use core::panic;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::{broadcast, mpsc};
use tokio::time;
use tracing::{Level, debug, info, debug_span, warn, trace};
use tracing::Instrument;

enum AutonomyModeSignal {
    // TODO: Warn of getting unscheduled
}

#[async_trait]
pub trait AutonomyMode: Send + Sync + Serialize + 'static {
    fn name(&self) -> String;
    fn activation(&self) -> Option<Activation>;
    fn priority(&self) -> u8;
    async fn run(
        &mut self,
        mut rx_telem: broadcast::Receiver<Telemetry>,
        tx_command: mpsc::Sender<Command>,
        active: Arc<tokio::sync::Mutex<bool>>,
    ) -> Result<()>;
}

pub struct ManagedAutonomyMode {
    pub name: String,
    pub active: Arc<tokio::sync::Mutex<bool>>,
    pub priority: u8,
    pub activation: Option<Activation>,
    pub handle: tokio::task::JoinHandle<()>,
}

pub struct Resolver {
    telem_buffer: Arc<Mutex<VecDeque<Telemetry>>>,
    vars: HashMap<String, Variable>,
}
impl Resolvable for Resolver {
    fn get_telemetry(&self, var: &str) -> Option<Variable> {
        let telem_buffer = self.telem_buffer.lock().unwrap();
        if let Some(latest_telem) = telem_buffer.front() {
            match var {
                "proximity_m" => Some(Variable::Float64(Value::Literal(
                    latest_telem.proximity_m as f64,
                ))),
                "timestamp" => Some(Variable::Float64(Value::Literal(
                    latest_telem.timestamp as f64,
                ))),
                _ => None,
            }
        } else {
            None
        }
    }
    fn get_variable(&self, var: &str) -> Option<Variable> {
        self.vars.get(var).cloned()
    }
}

pub struct Router<TR>
where
    TR: Transport<Telemetry, Command>,
{
    engagement_mode: EngagementMode,
    transport: TR,
    tx_telem_to_modes: broadcast::Sender<Telemetry>,
    rx_telem_in_modes: broadcast::Receiver<Telemetry>,
    observability: Arc<obs::ObservabilitySubsystem>,
    selected_mode: Option<String>,
    autonomy_modes: HashMap<String, (ManagedAutonomyMode, mpsc::Receiver<Command>)>,
    resolver: Resolver,
}

impl<TR> Router<TR>
where
    TR: Transport<Telemetry, Command>,
{
    pub fn new(
        transport: TR,
        observability: Arc<obs::ObservabilitySubsystem>,
        config: &Config,
    ) -> Self {
        debug!("Initializing Router with config: {:?}", config.router);
        let (tx_telem_to_modes, rx_telem_in_modes) =
            broadcast::channel(config.router.max_autonomy_modes);
        Self {
            engagement_mode: EngagementMode::Off,
            transport: transport,
            tx_telem_to_modes,
            rx_telem_in_modes,
            observability,
            selected_mode: None, // TODO: Figure out a better way to sequence start up and initial configuration.  Best to just rely on the routing rules to determine which Mode activates first?
            autonomy_modes: HashMap::new(),
            resolver: Resolver {
                telem_buffer: Arc::new(Mutex::new(VecDeque::with_capacity(
                    config.router.historic_telem_buffer_size,
                ))),
                vars: HashMap::new(),
            },
        }
    }

    pub fn register_autonomy_mode<M: AutonomyMode>(&mut self, mut mode: M, config: &Config) {
        let (tx_command_to_router, rx_command_from_modes) =
            mpsc::channel::<Command>(config.router.command_channel_buffer_size);
        let mode_name = mode.name().clone();
        let priority = mode.priority().clone();
        let activation = mode.activation().clone();
        let active = Arc::new(tokio::sync::Mutex::new(false));
        let active_clone = active.clone();
        let rx_telem_in_mode = self.rx_telem_in_modes.resubscribe();
        let mode_str = serde_json::to_string_pretty(&mode).unwrap();
        let mode_name_clone = mode_name.clone();
        let handle = tokio::spawn(async move {
            // TODO: Make thread/process
            if let Err(e) = mode
                .run(rx_telem_in_mode, tx_command_to_router.clone(), active_clone)
                .await
            {
                warn!("Autonomy Mode error: {}", e);
            }
        }.instrument(debug_span!("run", autonomy_mode = %mode_name_clone)));
        let managed_mode = ManagedAutonomyMode {
            name: mode_name.clone(),
            priority,
            activation,
            active,
            handle,
        };
        self.autonomy_modes
            .insert(mode_name.clone(), (managed_mode, rx_command_from_modes));
        println!("Registered Autonomy Mode: {}", mode_name);
    }

    pub async fn run(&mut self, config: &Config) -> Result<()> {
        info!("Router is starting");
        let mut routing_interval =
            time::interval(std::time::Duration::from_secs(config.router.period_seconds));
        let mut client_stream_write_halves = Vec::<Box<dyn WriteHalf<Command> + Send>>::new();
        loop {
            tokio::select! {
                // Accept new connections from C2
                Ok(stream) = self.transport.accept() => {
                    info!("C2 connected to Router");
                    let (mut read_half, write_half) = stream.split();
                    client_stream_write_halves.push(Box::new(write_half));

                    // Spawn read handler
                    let observability = self.observability.clone();
                    let telem_buffer = self.resolver.telem_buffer.clone();
                    let tx_telem_to_modes = self.tx_telem_to_modes.clone();
                    let config = config.clone();
                    tokio::spawn(async move {
                      loop {
                        match read_half.read().await {
                          Ok(telemetry) => {
                              // handle msg from clients
                              observability.log_event(
                                  obs::Location::Router,
                                  obs::Event::TelemetryReceived(telemetry.clone()),
                              );
                              let mut telem_buffer = telem_buffer.lock().unwrap();
                              telem_buffer.push_front(telemetry.clone());
                              if telem_buffer.len() > config.router.historic_telem_buffer_size {
                                telem_buffer.pop_back();
                              }
                              if let Err(_) = tx_telem_to_modes.send(telemetry) {
                                  warn!("No active subscribers for telemetry");
                              }
                          }
                          Err(e) => break, // Connection closed by client
                        }
                      }
                    });
                }

                // Forward commands from active mode to C2
                // TODO: There is an issue here where modes can send a bunch of commands when inactive and then when the become active we'll forward them along
                // Need to flush the channels when switching modes
                Some(command) = async {
                  if let Some(mode_name) = &self.selected_mode {
                    self.autonomy_modes.get_mut(mode_name).unwrap().1.recv().await
                  } else {
                    futures::future::pending().await
                  }
                } => {
                    self.observability.log_event(
                        obs::Location::AutonomyMode(self.selected_mode.clone().unwrap()),
                        obs::Event::CommandIssued { // TODO: Have this include which mode issued it
                            commands: vec![command.clone()],
                            reason: None,
                        },
                    );
                    let mut idx_to_remove = vec![];
                    for (idx, write_half) in client_stream_write_halves.iter_mut().enumerate() {
                        if let Err(_) = write_half.write(command.clone()).await {
                            // Connection closed by client so clean up
                            // Reverse order to not mess up indices when removing
                            idx_to_remove.insert(0, idx);
                        }
                    }
                    for idx in idx_to_remove {
                        client_stream_write_halves.remove(idx);
                    }
                }

                // Updating Routing Decision
                _ = routing_interval.tick() => {
                  let mut candidate_modes: Vec<(u8, String)> = vec![];
                  // Determine if current mode has a Histeretic activation, if so, evaluate exit criteria
                  if let Some(current_mode_name) = &self.selected_mode {
                    let (current_mode, _) = self.autonomy_modes.get(current_mode_name).unwrap(); // TODO: Handle when no match between current_mode and index.  Add test.
                    if let Some(Activation::Hysteretic { enter: _, exit }) = &current_mode.activation {
                      // Evaluate exit criteria
                      match exit.eval(&self.resolver) {
                        Ok(exit_met) => {
                          if !exit_met {
                            continue;
                          }
                        }
                        Err(e) => {
                          warn!("Failed to evaluate exit criteria for mode {}: {:?}.  Exiting.", current_mode_name, e); // TODO: Figure out what desired behavior is here (and make configurable)
                          continue;
                        }
                      }
                    }
                  }
                  // Else find highest priority mode whose activation criteria is met and switch to it
                  for (mode_name, (managed_mode, _)) in &self.autonomy_modes {
                    if let Some(activation) = &managed_mode.activation {
                      let activation = match activation {
                        Activation::Immediate(expr) => expr,
                        Activation::Hysteretic { enter, exit: _ } => enter,
                      };
                      match activation.eval(&self.resolver) {
                        Ok(activation_met) => {
                          if activation_met {
                            candidate_modes.push((managed_mode.priority, mode_name.clone()));
                          }
                        }
                        Err(e) => {
                          warn!("Failed to evaluate activation criteria for mode {}: {:?}.  Mode will not be activated.", mode_name, e);
                        }
                      }
                    }
                  }
                  if !candidate_modes.is_empty() {
                    candidate_modes.sort_by(|a, b| b.0.cmp(&a.0)); // Descending
                    let highest_priority_mode = &candidate_modes[0].1;
                    if Some(highest_priority_mode.clone()) != self.selected_mode {
                      info!("Switching to mode: {}", highest_priority_mode);
                      { // Atomically set active flags
                        if let Some(current_mode_name) = &self.selected_mode {
                          let (mode, _) = self.autonomy_modes.get_mut(current_mode_name).unwrap();
                          let mut active = mode.active.lock().await;
                          *active = false;
                        }
                        let (mode, _) = self.autonomy_modes.get_mut(highest_priority_mode).unwrap();
                        let mut active = mode.active.lock().await;
                        *active = true;
                      }
                      self.selected_mode = Some(highest_priority_mode.clone());
                    }
                  }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use crate::definitions::{Expr, Value, Variable};

    use super::*;

    #[test]
    fn test_eval() {
        let resolver = Resolver {
            telem_buffer: Arc::new(Mutex::new(VecDeque::from(vec![
                Telemetry {
                    timestamp: 1000,
                    proximity_m: 150,
                },
                Telemetry {
                    timestamp: 2000,
                    proximity_m: 50,
                },
            ]))),
            vars: HashMap::from_iter([
                ("a".into(), Variable::Float64(Value::Literal(49.0))),
                ("b".into(), Variable::Bool(Value::Literal(true))),
                (
                    "c".into(),
                    Variable::String(Value::Literal("test".to_string())),
                ),
            ]),
        };

        assert_eq!(
            Expr::GreaterThan(
                Box::new(Expr::Term(Variable::Float64(Value::Literal(100.0)))),
                Box::new(Expr::Term(Variable::Float64(Value::Literal(100.0)))),
            )
            .eval(&resolver)
            .unwrap(),
            false
        );

        assert_eq!(
            Expr::GreaterThan(
                Box::new(Expr::Term(Variable::Float64(Value::Literal(100.0)))),
                Box::new(Expr::Term(Variable::Float64(Value::Literal(101.0)))),
            )
            .eval(&resolver)
            .unwrap(),
            false
        );

        assert_eq!(
            Expr::GreaterThan(
                Box::new(Expr::Term(Variable::Float64(Value::Literal(101.0)))),
                Box::new(Expr::Term(Variable::Float64(Value::Literal(100.0)))),
            )
            .eval(&resolver)
            .unwrap(),
            true
        );

        assert_eq!(
            Expr::GreaterThan(
                Box::new(Expr::Term(Variable::Float64(Value::TelemetryRef(
                    "proximity_m".to_string()
                )))),
                Box::new(Expr::Term(Variable::Float64(Value::VariableRef(
                    "a".to_string()
                )))),
            )
            .eval(&resolver)
            .unwrap(),
            true
        );
        assert_eq!(
            Expr::GreaterThan(
                Box::new(Expr::Term(Variable::Float64(Value::VariableRef(
                    "a".to_string()
                )))),
                Box::new(Expr::Term(Variable::Float64(Value::TelemetryRef(
                    "proximity_m".to_string()
                )))),
            )
            .eval(&resolver)
            .unwrap(),
            false
        );

        assert_eq!(
            Expr::Term(Variable::Bool(Value::Literal(true)))
                .eval(&resolver)
                .unwrap(),
            true
        );
        assert_eq!(
            Expr::Term(Variable::Bool(Value::Literal(false)))
                .eval(&resolver)
                .unwrap(),
            false
        );
        assert_eq!(
            Expr::Term(Variable::String(Value::Literal("true".to_string())))
                .eval(&resolver)
                .unwrap(),
            true
        );
        assert_eq!(
            Expr::Term(Variable::String(Value::Literal("false".to_string())))
                .eval(&resolver)
                .unwrap(),
            true
        );
        assert_eq!(
            Expr::Term(Variable::String(Value::Literal("".to_string())))
                .eval(&resolver)
                .unwrap(),
            false
        );
        assert_eq!(
            Expr::Term(Variable::Float64(Value::Literal(0.0)))
                .eval(&resolver)
                .unwrap(),
            false
        );
        assert_eq!(
            Expr::Term(Variable::Float64(Value::Literal(1.0)))
                .eval(&resolver)
                .unwrap(),
            true
        );

        assert_eq!(
            Expr::GreaterThan(
                Box::new(Expr::Term(Variable::Bool(Value::Literal(true)))),
                Box::new(Expr::Term(Variable::Float64(Value::Literal(0.0)))),
            )
            .eval(&resolver)
            .unwrap(),
            true
        );
        assert_eq!(
            Expr::GreaterThan(
                Box::new(Expr::Term(Variable::Bool(Value::VariableRef(
                    "b".to_string()
                )))),
                Box::new(Expr::Term(Variable::Float64(Value::TelemetryRef(
                    "proximity_m".to_string()
                )))),
            )
            .eval(&resolver)
            .unwrap(),
            false
        );
        assert_eq!(
            Expr::Equal(
                Box::new(Expr::Term(Variable::Bool(Value::Literal(true)))),
                Box::new(Expr::Term(Variable::Float64(Value::Literal(1.0)))),
            )
            .eval(&resolver)
            .unwrap(),
            true
        );

        assert_eq!(
            Expr::And(Vec::from([
                Expr::Term(Variable::Bool(Value::Literal(true))),
                Expr::Term(Variable::Float64(Value::Literal(1.0))),
                Expr::GreaterThan(
                    Box::new(Expr::Term(Variable::Float64(Value::Literal(2.0)))),
                    Box::new(Expr::Term(Variable::Float64(Value::Literal(1.0)))),
                )
            ]))
            .eval(&resolver)
            .unwrap(),
            true
        );
        assert_eq!(
            Expr::And(Vec::from([
                Expr::Term(Variable::Bool(Value::Literal(true))),
                Expr::Term(Variable::Float64(Value::Literal(1.0))),
                Expr::GreaterThan(
                    Box::new(Expr::Term(Variable::Float64(Value::Literal(1.0)))),
                    Box::new(Expr::Term(Variable::Float64(Value::Literal(2.0)))),
                )
            ]))
            .eval(&resolver)
            .unwrap(),
            false
        );

        assert_eq!(
            Expr::And(Vec::from([
                Expr::Term(Variable::Bool(Value::Literal(true))),
                Expr::Term(Variable::Float64(Value::Literal(1.0))),
                Expr::Or(Vec::from([
                    Expr::Term(Variable::Bool(Value::Literal(false))),
                    Expr::Term(Variable::Bool(Value::Literal(true))),
                ])),
            ]))
            .eval(&resolver)
            .unwrap(),
            true
        );

        // Test type mismatches
        let result = Expr::Equal(
            Box::new(Expr::Term(Variable::String(Value::Literal(
                "test".to_string(),
            )))),
            Box::new(Expr::Term(Variable::Bool(Value::Literal(true)))),
        )
        .eval(&resolver);
        assert!(result.is_err());
        let error: crate::definitions::Error = result.unwrap_err();
        assert!(matches!(error, crate::definitions::Error::TypeMismatch));
    }
}
