use anyhow::Result;
use async_trait::async_trait;
use core::panic;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tokio::time;
use tracing::{debug, info, warn};
use crate::c2::{Command, Telemetry};
use crate::config::{Config, EngagementMode};
use crate::definitions::{Activation, Expr, GenericVariable, Variable};
use crate::observability as obs;


enum AutonomyModeSignal {
    // TODO: Warn of getting unscheduled
}

#[async_trait]
pub trait AutonomyMode: Send + Sync {
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

pub struct Router {
    engagement_mode: EngagementMode,
    rx_telem: mpsc::Receiver<Telemetry>,
    tx_telem_to_modes: broadcast::Sender<Telemetry>,
    rx_telem_in_modes: broadcast::Receiver<Telemetry>,
    tx_command: mpsc::Sender<Command>,
    observability: Arc<obs::ObservabilitySubsystem>,
    selected_mode: Option<String>,
    autonomy_modes: HashMap<String, (ManagedAutonomyMode, mpsc::Receiver<Command>)>,
    telem_buffer: VecDeque<Telemetry>,
}

impl Router {
    pub fn new(
        rx_telem: mpsc::Receiver<Telemetry>,
        tx_command: mpsc::Sender<Command>,
        observability: Arc<obs::ObservabilitySubsystem>,
        config: &Config,
    ) -> Self {
        debug!("Initializing Router with config: {:?}", config.router);
        let (tx_telem_to_modes, rx_telem_in_modes) =
            broadcast::channel(config.router.max_autonomy_modes);
        Self {
            engagement_mode: EngagementMode::Off,
            rx_telem,
            tx_telem_to_modes,
            rx_telem_in_modes,
            tx_command,
            observability,
            selected_mode: None, // TODO: Figure out a better way to sequence start up and initial configuration.  Best to just rely on the routing rules to determine which Mode activates first?
            autonomy_modes: HashMap::new(),
            telem_buffer: VecDeque::with_capacity(config.router.historic_telem_buffer_size),
        }
    }

    pub fn register_autonomy_mode<M: AutonomyMode + 'static>(&mut self, mut mode: M, config: &Config) {
        let (tx_command_to_router, rx_command_from_modes) =
            mpsc::channel::<Command>(config.router.command_channel_buffer_size);
        let mode_name = mode.name().clone();
        let priority = mode.priority().clone();
        let activation = mode.activation().clone();
        let active = Arc::new(tokio::sync::Mutex::new(false));
        let active_clone = active.clone();
        let rx_telem_in_mode = self.rx_telem_in_modes.resubscribe();
        let handle = tokio::spawn(async move {
            // TODO: Make thread/process
            if let Err(e) = mode
                .run(rx_telem_in_mode, tx_command_to_router.clone(), active_clone)
                .await
            {
                warn!("Autonomy Mode error: {}", e);
            }
        });
        let managed_mode = ManagedAutonomyMode {
            name: mode_name.clone(),
            priority,
            activation,
            active,
            handle,
        };
        self.autonomy_modes
            .insert(mode_name.clone(), (managed_mode, rx_command_from_modes));
    }

    pub async fn run(&mut self, config: &Config) -> Result<()> {
        info!("Router is starting");
        let mut routing_interval =
            time::interval(std::time::Duration::from_secs(config.router.period_seconds));
        loop {
            tokio::select! {
                // Receive telemetry from C2 and forward to modes
                Some(telemetry) = self.rx_telem.recv() => {
                    self.observability.log_event(
                        obs::Location::Router,
                        obs::Event::TelemetryReceived(telemetry.clone()),
                    );
                    self.telem_buffer.push_front(telemetry.clone());
                    if self.telem_buffer.len() > config.router.historic_telem_buffer_size {
                      self.telem_buffer.pop_back();
                    }
                    if let Err(_) = self.tx_telem_to_modes.send(telemetry) {
                        warn!("No active subscribers for telemetry");
                    }
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
                    if let Err(_) = self.tx_command.send(command).await {
                        warn!("No active subscribers for commands");
                    }
                }

                // Updating Routing Decision
                _ = routing_interval.tick() => {
                  let latest_telem = self.telem_buffer.front().cloned();
                  let mut candidate_modes: Vec<(u8, String)> = vec![];
                  // Determine if current mode has a Histeretic activation, if so, evaluate exit criteria
                  if let Some(current_mode_name) = &self.selected_mode {
                    let (current_mode, _) = self.autonomy_modes.get(current_mode_name).unwrap(); // TODO: Handle when no match between current_mode and index.  Add test.
                    if let Some(Activation::Hysteretic { enter: _, exit }) = &current_mode.activation {
                      // Evaluate exit criteria
                      if !eval_activation_expr(exit, &latest_telem) {
                        continue;
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
                      if eval_activation_expr(activation, &latest_telem) {
                        candidate_modes.push((managed_mode.priority, mode_name.clone()));
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

                    } else {
                      debug!("Staying in mode: {}", highest_priority_mode);
                    }
                  }
                }
            }
        }
    }
}

// TODO: Write comprehensive unit tests
// TODO: Make this not panic
fn eval_activation_expr(expr: &Expr, latest_telem: &Option<Telemetry>) -> bool {
    match expr {
        Expr::Var(v) => {
            true // TODO: Fixme
        }
        Expr::And(clauses) => {
            for clause in clauses {
                if !eval_activation_expr(clause, latest_telem) {
                    return false;
                }
            }
            true
        }
        Expr::Or(clauses) => {
            for clause in clauses {
                if eval_activation_expr(clause, latest_telem) {
                    return true;
                }
            }
            false
        }
        Expr::Not(clause) => !eval_activation_expr(clause, latest_telem),
        Expr::GreaterThan(var1, var2) => {
            return match (&**var1, &**var2) {
                (Variable::Float64(var1), Variable::Float64(var2)) => {
                    let a = match var1 {
                        GenericVariable::Literal(v) => *v,
                        GenericVariable::TelemetryRef(name) => {
                            if let Some(telem) = &latest_telem {
                                match name.as_str() {
                                    "proximity_m" => telem.proximity_m as f64,
                                    _ => return false, // Unknown telemetry field
                                }
                            } else {
                                return false; // No telemetry available
                            }
                        }
                        _ => return false, // TODO: Implement rest
                    };
                    let b = match var2 {
                        GenericVariable::Literal(v) => *v,
                        GenericVariable::TelemetryRef(name) => {
                            if let Some(telem) = &latest_telem {
                                match name.as_str() {
                                    "proximity_m" => telem.proximity_m as f64,
                                    _ => return false, // Unknown telemetry field
                                }
                            } else {
                                return false; // No telemetry available
                            }
                        }
                        _ => return false, // TODO: Implement rest
                    };
                    println!("Comparing {} > {}", a, b);
                    a > b
                }
                _ => false, // TODO: Implement rest
            };
        }
        _ => false, // TODO: Implement rest
    }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_eval() {
    assert_eq!(eval_activation_expr(&Box::new(Expr::GreaterThan(
        Box::new(Variable::Float64(GenericVariable::Literal(100.0))),
        Box::new(Variable::Float64(GenericVariable::Literal(100.0))),
    )), &None), false);
    assert_eq!(eval_activation_expr(&Box::new(Expr::GreaterThan(
        Box::new(Variable::Float64(GenericVariable::Literal(100.0))),
        Box::new(Variable::Float64(GenericVariable::Literal(101.0))),
    )), &None), false);
    assert_eq!(eval_activation_expr(&Box::new(Expr::GreaterThan(
        Box::new(Variable::Float64(GenericVariable::Literal(101.0))),
        Box::new(Variable::Float64(GenericVariable::Literal(100.0))),
    )), &None), true);
    assert_eq!(eval_activation_expr(&Expr::Not(Box::new(Expr::GreaterThan(
        Box::new(Variable::Float64(GenericVariable::Literal(101.0))),
        Box::new(Variable::Float64(GenericVariable::Literal(100.0))),
    ))), &None), false);
  }
}