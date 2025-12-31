use crate::c2::{AutonomyModeMessage, RouterMessage};
use crate::config::{Config, EngagementMode};
use crate::definitions::Variable;
use crate::definitions::{Activation, Resolvable, Value};
use crate::{observability as obs, utils};
use crate::transports::{MpscTransport, ReadHalf};
use crate::transports::Stream;
use crate::transports::Transport;
use crate::transports::WriteHalf;
use anyhow::Result;
use async_trait::async_trait;
use rand::Rng;
use core::panic;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::time;
use tracing::{debug, info, debug_span, warn};
use tracing::Instrument;

#[async_trait]
pub trait AutonomyMode<T, C>: Send + Sync + Serialize + 'static
where
  T: Clone + Serialize + for<'de>Deserialize<'de> + Send + 'static,
  C: Clone + Serialize + for<'de>Deserialize<'de> + Send + 'static,
{
    fn name(&self) -> String;
    fn activation(&self) -> Activation;
    fn priority(&self) -> u8;
    async fn run(&mut self, stream: Box<dyn Stream<AutonomyModeMessage<T>, RouterMessage<C>>>) -> Result<()>;
}

pub struct ManagedAutonomyMode {
    name: String,
    active: bool,
    priority: u8,
    activation: Activation,
    handle: tokio::task::JoinHandle<()>,
}

pub struct Resolver<T> {
    telem_buffer: Arc<Mutex<VecDeque<T>>>,
    vars: HashMap<String, Variable>,
}
impl<T> Resolvable for Resolver<T> 
where 
  T: Serialize,
{
    fn get_telemetry_point(&self, var: &str) -> Option<Variable> {
        let telem_buffer = self.telem_buffer.lock().unwrap();
        if let Some(latest_telem) = telem_buffer.front() {
            let map = serde_json::to_value(latest_telem).ok()?; // TODO: Figure out a better way
            match map.get(var).cloned() {
                Some(serde_json::Value::Number(n)) => n.as_f64().map(|v| Variable::Float64(Value::Literal(v))),
                Some(serde_json::Value::Bool(b)) => Some(Variable::Bool(Value::Literal(b))),
                Some(serde_json::Value::String(s)) => Some(Variable::String(Value::Literal(s))),
                None => None,
                _ => None,
            }
        } else {
            None
        }
    }
    fn get_telemetry_points(&self, var: &str, points: usize) -> Vec<Variable> {
        let telem_buffer = self.telem_buffer.lock().unwrap();
        let mut result = Vec::new();
        for telem_point in telem_buffer.range(..points) {
            let map = serde_json::to_value(telem_point).unwrap(); // TODO: Figure out a better way
            match map.get(var).cloned() {
                Some(serde_json::Value::Number(n)) => {
                    if let Some(v) = n.as_f64() {
                        result.push(Variable::Float64(Value::Literal(v)));
                    }
                }
                Some(serde_json::Value::Bool(b)) => {
                    result.push(Variable::Bool(Value::Literal(b)));
                }
                Some(serde_json::Value::String(s)) => {
                    result.push(Variable::String(Value::Literal(s)));
                }
                None => {}
                _ => {}
            }
        }
        result
    }
    fn get_variable(&self, var: &str) -> Option<Variable> {
        self.vars.get(var).cloned()
    }
}

pub struct Build;
pub struct Run;

pub struct Router<T, C, State = Build> {
    _state: std::marker::PhantomData<State>,
    engagement_mode: EngagementMode,
    c2_transport: Box<dyn Transport<T, C>>,
    autonomy_modes_transport: Box<dyn Transport<RouterMessage<C>, AutonomyModeMessage<T>>>,
    observability: Arc<obs::ObservabilitySubsystem<T, C>>,
    selected_mode: Option<(String, u64)>, // (mode name, comms nonce)
    autonomy_modes: HashMap<String, ManagedAutonomyMode>,
    autonomy_mode_write_handles: Arc<tokio::sync::Mutex<HashMap<String, Box<dyn WriteHalf<AutonomyModeMessage<T>>>>>>,
    autonomy_mode_read_handles: Arc<tokio::sync::Mutex<HashMap<String, Box<dyn ReadHalf<RouterMessage<C>>>>>>,
    resolver: Resolver<T>,
}

impl<T, C> Router<T, C, Build> 
where
  T: Clone + Serialize + for<'de>Deserialize<'de> + Send + 'static + std::marker::Sync,
  C: Clone + Serialize + for<'de>Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    pub fn new(
        c2_transport: Box<dyn Transport<T, C>>,
        observability: Arc<obs::ObservabilitySubsystem<T, C>>,
        config: &Config,
    ) -> Self {
        debug!("Initializing Router with config: {:?}", config.router);
        let autonomy_modes_transport: MpscTransport<RouterMessage<C>, AutonomyModeMessage<T>> = MpscTransport::new(config.router.command_channel_buffer_size); // TODO: Make transport configurable
        Self {
            _state: std::marker::PhantomData,
            engagement_mode: EngagementMode::Off,
            c2_transport,
            autonomy_modes_transport: Box::new(autonomy_modes_transport),
            observability,
            selected_mode: None, // TODO: Figure out a better way to sequence start up and initial configuration.  Best to just rely on the routing rules to determine which Mode activates first?
            autonomy_modes: HashMap::new(),
            autonomy_mode_write_handles: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            autonomy_mode_read_handles: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            resolver: Resolver {
                telem_buffer: Arc::new(Mutex::new(VecDeque::with_capacity(
                    config.router.historic_telem_buffer_size,
                ))),
                vars: HashMap::new(),
            },
        }
    }
    pub fn c2_transport(mut self, transport: impl Transport<T, C> + 'static) -> Self {
      self.c2_transport = Box::new(transport);
      self
    }
    // TODO: Build out rest of configurability here as continuous of builder pattern rather than constructor params

    pub async fn run(mut self, config: &Config) -> Result<Router<T, C, Run>> {
        info!("Router is starting");
        self.update_active_mode().await;
        let mut routing_interval =
            time::interval(std::time::Duration::from_secs(config.router.period_seconds));
        let mut client_stream_write_halves = Vec::<Box<dyn WriteHalf<C>>>::new();
        let mut client_stream_read_halves = Vec::<Box<dyn ReadHalf<T>>>::new();
        loop {
            tokio::select! {
                // Accept new connections from C2
                Ok(stream) = self.c2_transport.accept() => {
                    info!("C2 connected to Router");
                    let (read_half, write_half) = stream.split();
                    client_stream_write_halves.push(write_half);
                    client_stream_read_halves.push(read_half);
                }

                // Handle incoming messages from C2
                Some((result, idx)) = async {
                    if client_stream_read_halves.is_empty() {
                        return None;
                    }
                    let mut futures = Vec::new();
                    for (i, read_half) in client_stream_read_halves.iter_mut().enumerate() {
                        futures.push(Box::pin(async move { (read_half.read().await, i) }));
                    }
                    Some(futures::future::select_all(futures).await.0)
                } => {
                    match result {
                        Ok(telemetry) => {
                            self.observability.log_event(
                                obs::Location::Router,
                                obs::Event::TelemetryReceived(telemetry.clone()),
                            );

                            {
                              let mut telem_buffer = self.resolver.telem_buffer.lock().unwrap();
                              telem_buffer.push_front(telemetry.clone());
                              if telem_buffer.len() > config.router.historic_telem_buffer_size {
                                telem_buffer.pop_back();
                              }
                            }

                            // Re-eval activations and update active mode prior to passing along telemetry in case mode switch is needed
                            self.update_active_mode().await;
                            
                            // Broadcast to all registered autonomy modes
                            for (mode_name, handle) in self.autonomy_mode_write_handles.lock().await.iter_mut() {
                                if let Err(_) = handle.write(AutonomyModeMessage::Telemetry(telemetry.clone())).await {
                                    warn!("Failed to forward telemetry to autonomy mode {}", mode_name);
                                }
                            }
                        }
                        Err(e) => {
                            // Connection closed, remove this client
                            client_stream_read_halves.remove(idx);
                        }
                    }
                }

                // Forward commands from active mode to C2
                command = async {
                  if let Some((mode_name, current_nonce)) = &self.selected_mode {
                    loop {
                      match self.autonomy_mode_read_handles.lock().await.get_mut(mode_name).expect("Mode not found").read().await {
                        Ok(message) => {
                          match message {
                            RouterMessage::Command { data, nonce } => {
                              if nonce == *current_nonce {
                                break data;
                              }
                            },
                          };
                        }
                        Err(_) => {
                          warn!("Failed to read command from active mode {}", mode_name);
                        }
                      }
                    }
                  } else {
                    futures::future::pending().await
                  }
                } => {
                    self.observability.log_event(
                        obs::Location::AutonomyMode(self.selected_mode.clone().unwrap().0.clone()),
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
                _ = routing_interval.tick() => self.update_active_mode().await,
            }
        }
    }

    async fn update_active_mode(&mut self) {
      let mut candidate_modes: Vec<(u8, String)> = vec![];
      // Determine if current mode has a Histeretic activation, if so, evaluate exit criteria
      if let Some((current_mode_name, _)) = &self.selected_mode {
        let current_mode = self.autonomy_modes.get(current_mode_name).unwrap(); // TODO: Handle when no match between current_mode and index.  Add test.
        if let Activation::Hysteretic { enter: _, exit } = &current_mode.activation {
          // Evaluate exit criteria
          match exit.eval(&self.resolver) {
            Ok(exit_met) => {
              if !exit_met {
                return;
              }
            }
            Err(e) => {
              warn!("Failed to evaluate exit criteria for mode {}: {:?}.  Exiting.", current_mode_name, e); // TODO: Figure out what desired behavior is here (and make configurable)
              return;
            }
          }
        }
      }
      // Else find highest priority mode whose activation criteria is met and switch to it
      for (mode_name, managed_mode) in &self.autonomy_modes {
        let activation = match &managed_mode.activation {
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
      if !candidate_modes.is_empty() {
        candidate_modes.sort_by(|a, b| b.0.cmp(&a.0)); // Descending
        let highest_priority_mode = &candidate_modes[0].1;
        if self.selected_mode.is_none() || highest_priority_mode.clone() != *self.selected_mode.clone().unwrap().0 {
          info!("Switching to mode: {}", highest_priority_mode);
          let mut locked_write_handles = self.autonomy_mode_write_handles.lock().await;
          
          // Hand off to new mode, order matters
          // Deactivate current mode
          if let Some((current_mode_name, _)) = &self.selected_mode {
            let mode = self.autonomy_modes.get_mut(current_mode_name).expect("Mode not found");
            mode.active = false;
            let write_half = locked_write_handles.get_mut(current_mode_name).unwrap();
            write_half.write(AutonomyModeMessage::Inactive).await.expect("Failed to send deactivate message to mode");
          }
          
          // Activate new mode
          let nonce = rand::thread_rng().gen::<u64>();
          let mode = self.autonomy_modes.get_mut(highest_priority_mode).expect("Mode not found");
          mode.active = true;
          let write_half = locked_write_handles.get_mut(highest_priority_mode).unwrap();
          write_half.write(AutonomyModeMessage::Active { nonce }).await.expect("Failed to send activate message to mode");
          
          // Set selected mode
          self.selected_mode = Some((highest_priority_mode.clone(), nonce));
        }
      }
    }

}

impl<T, C> Router<T, C>
where
  T: Clone + Serialize + for<'de>Deserialize<'de> + Send + 'static + std::marker::Sync,
  C: Clone + Serialize + for<'de>Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    // TODO: Support registering and deregistering modes while running
    pub async fn register_autonomy_mode<M: AutonomyMode<T, C>>(&mut self, mut mode: M) -> Result<()> {
        let (client_stream, server_stream) = self.autonomy_modes_transport.channel().await?;
        let mode_name = mode.name().clone();
        let priority = mode.priority();
        let activation = mode.activation().clone();
        let mode_name_clone = mode_name.clone();
        let handle = tokio::spawn(async move {
            // TODO: Make thread/process
            // TODO: Implement restart on Err, with backoff?
            if let Err(e) = mode
                .run(client_stream)
                .await
            {
                warn!("Autonomy Mode error: {}", e);
            }
        }.instrument(debug_span!("run", autonomy_mode = %mode_name_clone)));
        let managed_mode = ManagedAutonomyMode {
            name: mode_name.clone(),
            priority,
            activation,
            active: false,
            handle,
        };
        self.autonomy_modes
            .insert(mode_name.clone(), managed_mode);
        let (read_half, write_half) = server_stream.split();
        self.autonomy_mode_write_handles.lock().await.insert(mode_name.clone(), write_half);
        self.autonomy_mode_read_handles.lock().await.insert(mode_name.clone(), read_half);
        println!("Registered Autonomy Mode: {}", mode_name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use crate::{c2::Telemetry, definitions::{Expr, Value, Variable}};

    use super::*;

    #[test]
    fn test_eval() {
        let resolver = Resolver {
            telem_buffer: Arc::new(Mutex::new(VecDeque::from(vec![
                Telemetry {
                  pointing_error: 1.0,
                  in_sunlight: true,
                },
                Telemetry {
                  pointing_error: 2.0,
                  in_sunlight: true,
                },
                Telemetry {
                  pointing_error: 3.0,
                  in_sunlight: false,
                },
                Telemetry {
                  pointing_error: 3.0,
                  in_sunlight: false,
                },
                Telemetry {
                  pointing_error: 3.0,
                  in_sunlight: false,
                },
            ]))),
            vars: HashMap::from_iter([
                ("a".into(), Variable::Float64(Value::Literal(0.9))),
                ("b".into(), Variable::Bool(Value::Literal(false))),
                (
                    "c".into(),
                    Variable::String(Value::Literal("test".to_string())),
                ),
            ]),
        };

        // Assert understanding of VecDeque
        assert_eq!(
          resolver.get_telemetry_point("pointing_error"),
          Some(Variable::Float64(Value::Literal(1.0)))
        );

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
                    "pointing_error".to_string()
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
                    "pointing_error".to_string()
                )))),
            )
            .eval(&resolver)
            .unwrap(),
            false
        );
        
        assert_eq!(
            Expr::Equal(
                Box::new(Expr::Term(Variable::Float64(Value::Literal(
                    1.5
                )))),
                Box::new(Expr::Term(Variable::Float64(Value::AverageTelemetryRef{
                    name: "pointing_error".to_string(),
                    points: 2,
                }))),
            )
            .eval(&resolver)
            .unwrap(),
            true
        );
        assert_eq!(
            Expr::Equal(
                Box::new(Expr::Term(Variable::Bool(Value::Literal(
                    true
                )))),
                Box::new(Expr::Term(Variable::Bool(Value::AverageTelemetryRef{
                    name: "in_sunlight".to_string(),
                    points: 3,
                }))),
            )
            .eval(&resolver)
            .unwrap(),
            true
        );
        assert_eq!(
            Expr::Equal(
                Box::new(Expr::Term(Variable::Bool(Value::Literal(
                    false
                )))),
                Box::new(Expr::Term(Variable::Bool(Value::AverageTelemetryRef{
                    name: "in_sunlight".to_string(),
                    points: 5,
                }))),
            )
            .eval(&resolver)
            .unwrap(),
            true
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
            Expr::Term(Variable::Bool(Value::AverageTelemetryRef{
                name: "in_sunlight".to_string(),
                points: 3,
            })).eval(&resolver).unwrap(),
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
                    "pointing_error".to_string()
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
