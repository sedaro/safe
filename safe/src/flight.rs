use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::definitions::{Activation, Expr, Resolvable, Value, Variable};
use crate::telemetry_frame::TelemetryFrame;
use crate::{AutonomyModeId, AutonomyModeMeta};

const TELEMETRY_HISTORY_LIMIT: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomyModeActivation {
    pub id: AutonomyModeId,
    #[serde(default)]
    pub activation: Option<Activation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flight {
    running: bool,
    halted: bool,
    fault: Option<String>,
    last_seq_applied: u64,
    active_autonomy_mode: Option<AutonomyModeId>,
    #[serde(default)]
    manual_active_override: Option<AutonomyModeId>,
    autonomy_modes: Vec<AutonomyModeMeta>,
    #[serde(default)]
    autonomy_mode_activations: Vec<AutonomyModeActivation>,
    #[serde(skip)]
    telemetry_history: VecDeque<TelemetryFrame>,
    last_planned_autonomy_mode: Option<AutonomyModeId>,
}

impl Default for Flight {
    fn default() -> Self {
        Self {
            running: true,
            halted: false,
            fault: None,
            last_seq_applied: 0,
            active_autonomy_mode: None,
            manual_active_override: None,
            autonomy_modes: vec![],
            autonomy_mode_activations: vec![],
            telemetry_history: VecDeque::new(),
            last_planned_autonomy_mode: None,
        }
    }
}

impl Flight {
    pub fn get_autonomy_modes(&self) -> &Vec<AutonomyModeMeta> {
        &self.autonomy_modes
    }

    pub fn get_autonomy_modes_mut(&mut self) -> &mut Vec<AutonomyModeMeta> {
        &mut self.autonomy_modes
    }

    pub fn set_autonomy_modes(&mut self, modes: Vec<AutonomyModeMeta>) {
        self.autonomy_modes = modes;
    }

    pub fn set_autonomy_mode_activations(&mut self, activations: Vec<AutonomyModeActivation>) {
        self.autonomy_mode_activations = activations;
    }

    pub fn set_manual_active_override(&mut self, mode: AutonomyModeId) {
        self.manual_active_override = Some(mode);
    }

    pub fn set_last_planned_autonomy_mode(&mut self, mode: AutonomyModeId) {
        self.last_planned_autonomy_mode = Some(mode);
    }

    pub fn clear_last_planned_autonomy_mode(&mut self) {
        self.last_planned_autonomy_mode = None;
    }

    pub fn clear_manual_active_override(&mut self) {
        self.manual_active_override = None;
    }

    pub fn get_manual_active_override(&self) -> Option<AutonomyModeId> {
        self.manual_active_override
    }

    pub fn has_autonomy_mode(&self, mode: AutonomyModeId) -> bool {
        self.autonomy_modes.iter().any(|m| m.id == mode)
    }

    pub fn note_telemetry(&mut self, telemetry: &TelemetryFrame) {
        self.telemetry_history.push_back(telemetry.clone());
        while self.telemetry_history.len() > TELEMETRY_HISTORY_LIMIT {
            self.telemetry_history.pop_front();
        }
    }

    pub fn get_seq(&self) -> u64 {
        self.last_seq_applied
    }

    pub fn peak_next_seq(&self) -> u64 {
        self.last_seq_applied + 1
    }

    pub fn set_seq(&mut self, new_seq: u64) {
        self.last_seq_applied = new_seq;
    }

    pub fn recalculate_active_autonomy_mode(&mut self) -> Option<AutonomyModeId> {
        let previous_active = self.active_autonomy_mode;
        let candidate_modes = &self.autonomy_modes;

        if let Some(manual_mode) = self.manual_active_override {
            let is_enabled = self
                .autonomy_modes
                .iter()
                .find(|m| m.id == manual_mode)
                .is_some_and(|m| m.enabled);

            if is_enabled {
                self.active_autonomy_mode = Some(manual_mode);
                return previous_active;
            }

            self.manual_active_override = None;
        }

        let resolver = FlightResolver {
            telemetry_history: &self.telemetry_history,
            last_planned_autonomy_mode: &self.last_planned_autonomy_mode,
        };

        if let Some(current_id) = self.active_autonomy_mode
            && let Some(current_mode) = self.autonomy_modes.iter().find(|m| m.id == current_id)
            && current_mode.enabled
            && let Some(Activation::Hysteretic { exit, .. }) = self.activation_for_mode(current_id)
        {
            match exit.eval(&resolver) {
                Ok(false) => {
                    return previous_active;
                }
                Ok(true) => {}
                Err(e) => {
                    warn!(
                        "Failed to evaluate hysteretic exit for mode {:?}: {:?}. Keeping current mode.",
                        current_id, e
                    );
                    return previous_active;
                }
            }
        }

        self.active_autonomy_mode = candidate_modes
            .iter()
            .filter(|mode| mode.enabled)
            .filter(|mode| self.activation_enter_satisfied(mode.id, &resolver))
            .max_by_key(|p| p.priority)
            .map(|p| p.id);

        if (self.active_autonomy_mode != previous_active) {
            warn!(
                "Active autonomy mode changed from {:?} to {:?}",
                previous_active, self.active_autonomy_mode
            );
        }

        previous_active
    }

    pub fn get_active_autonomy_mode(&self) -> Option<AutonomyModeId> {
        self.active_autonomy_mode
    }

    pub fn is_halted(&self) -> bool {
        self.halted
    }

    pub fn halt(&mut self) {
        self.halted = true;
    }

    pub fn stop(&mut self) {
        self.running = false;
    }

    pub fn start(&mut self) {
        self.running = true;
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn set_fault(&mut self, fault_reason: String) {
        self.fault = Some(fault_reason);
    }

    pub fn get_fault(&self) -> &Option<String> {
        &self.fault
    }

    pub fn clear_fault(&mut self) {
        self.fault = None;
    }

    pub fn is_active_autonomy_mode(&self, check_mode: Option<AutonomyModeId>) -> bool {
        self.active_autonomy_mode == check_mode
    }

    pub fn clear_active_autonomy_mode(&mut self) -> Option<AutonomyModeId> {
        let previous_active = self.active_autonomy_mode;
        self.active_autonomy_mode = None;

        previous_active
    }

    pub fn set_active_autonomy_mode(
        &mut self,
        new_active: AutonomyModeId,
    ) -> Option<AutonomyModeId> {
        let previous_active = self.active_autonomy_mode;
        self.active_autonomy_mode = Some(new_active);

        previous_active
    }

    fn activation_for_mode(&self, mode_id: AutonomyModeId) -> Option<&Activation> {
        self.autonomy_mode_activations
            .iter()
            .find(|cfg| cfg.id == mode_id)
            .and_then(|cfg| cfg.activation.as_ref())
    }

    fn activation_enter_satisfied(
        &self,
        mode_id: AutonomyModeId,
        resolver: &impl Resolvable,
    ) -> bool {
        match self.activation_for_mode(mode_id) {
            None => true,
            Some(Activation::Immediate(expr)) => {
                self.eval_activation_expr(mode_id, "immediate", expr, resolver)
            }
            Some(Activation::Hysteretic { enter, .. }) => {
                self.eval_activation_expr(mode_id, "hysteretic.enter", enter, resolver)
            }
        }
    }

    fn eval_activation_expr(
        &self,
        mode_id: AutonomyModeId,
        label: &str,
        expr: &Expr,
        resolver: &impl Resolvable,
    ) -> bool {
        match expr.eval(resolver) {
            Ok(v) => v,
            Err(e) => {
                match e {
                    crate::definitions::Error::UndefinedLastPlannedAutonomyMode(_) => {}
                    e => {
                        warn!(
                            "Error evaluating activation {} for mode {:?}: {:?}",
                            label, mode_id, e
                        );
                    }
                }
                false
            }
        }
    }
}

struct FlightResolver<'a> {
    telemetry_history: &'a VecDeque<TelemetryFrame>,
    last_planned_autonomy_mode: &'a Option<AutonomyModeId>,
}

impl FlightResolver<'_> {
    fn latest_telemetry(&self) -> Option<&TelemetryFrame> {
        self.telemetry_history.back()
    }

    fn telemetry_value_by_path(t: &TelemetryFrame, path: &str) -> Option<serde_json::Value> {
        let root = &t.payload;
        path.split('.')
            .try_fold(root, |cursor, segment| {
                if let Ok(idx) = segment.parse::<usize>() {
                    cursor.get(idx)
                } else {
                    cursor.get(segment)
                }
            })
            .cloned()
    }

    fn json_to_variable(v: serde_json::Value) -> Option<Variable> {
        match v {
            serde_json::Value::Bool(b) => Some(Variable::Bool(Value::Literal(b))),
            serde_json::Value::Number(n) => {
                n.as_f64().map(|f| Variable::Float64(Value::Literal(f)))
            }
            serde_json::Value::String(s) => Some(Variable::String(Value::Literal(s))),
            _ => None,
        }
    }
}

impl Resolvable for FlightResolver<'_> {
    fn get_variable(&self, _name: &str) -> Option<Variable> {
        None
    }

    fn get_telemetry_point(&self, name: &str) -> Option<Variable> {
        let latest = self.latest_telemetry()?;
        let value = Self::telemetry_value_by_path(latest, name)?;
        Self::json_to_variable(value)
    }

    fn get_last_planned_autonomy_mode(&self) -> Option<Variable> {
        let mode = self.last_planned_autonomy_mode;
        if let Some(mode) = mode {
            Self::json_to_variable(serde_json::Value::String(format!("{}", mode.0)))
        } else {
            None
        }
    }

    fn get_telemetry_points(&self, name: &str, points: usize) -> Vec<Variable> {
        self.telemetry_history
            .iter()
            .rev()
            .filter_map(|t| Self::telemetry_value_by_path(t, name))
            .filter_map(Self::json_to_variable)
            .take(points)
            .collect()
    }
}
