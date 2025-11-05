use futures::StreamExt;
use serde::{Deserialize, Serialize};

// TODO: Get feedback from Team on all of this Ontology!
// TODO: SedaroTS here instead?  QK awareness would be awesome for telem

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AutonomyModeDefinition {
    pub name: String,
    pub priority: u8,
    pub activation: Option<Activation>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VariableDefinition<T> {
    pub name: String,
    pub initial_value: Option<T>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum GenericVariable<T> {
    Literal(T),
    VariableRef(String),
    TelemetryRef(String),
}
// impl<T: PartialEq> PartialEq for GenericVariable<T> {
//     fn eq(&self, other: &Self) -> bool {
//         match (self, other) {
//             (GenericVariable::Literal(a), GenericVariable::Literal(b)) => a == b,
//             _ => false,
//         }
//     }
// }
// impl<T: PartialOrd> PartialOrd for GenericVariable<T> {
//     fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
//         match (self, other) {
//             (GenericVariable::Literal(a), GenericVariable::Literal(b)) => a.partial_cmp(b),
//             _ => None,
//         }
//     }
// }

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Variable {
    String(GenericVariable<String>),
    Float64(GenericVariable<f64>),
    Bool(GenericVariable<bool>),
}
// impl PartialEq for Variable {
//     fn eq(&self, other: &Self) -> bool {
//         match (self, other) {
//             (Variable::Float64(a), Variable::Float64(b)) => a == b,
//             (Variable::String(a), Variable::String(b)) => a == b,
//             (Variable::Bool(a), Variable::Bool(b)) => a == b,
//             (Variable::Float64(a), Variable::Bool(b)) => a == (if *b { 1.0 } else { 0.0 }),
//             (Variable::Bool(a), Variable::Float64(b)) => (if *a { 1.0 } else { 0.0 }) == b,
//             // (Variable::Float64(a), Variable::Bool(b)) => a == b.into(),
//             // (Variable::Bool(a), Variable::Float64(b)) => a.into() == b,
//             _ => false,
//         }
//     }
// }
// impl PartialOrd for Variable {
//     fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
//         match (self, other) {
//             (Variable::Float64(a), Variable::Float64(b)) => a.partial_cmp(b),
//             (Variable::String(a), Variable::String(b)) => a.partial_cmp(b),
//             (Variable::Bool(a), Variable::Bool(b)) => a.partial_cmp(b),
//             (Variable::Float64(a), Variable::Bool(b)) => a.partial_cmp(&(if *b { 1.0 } else { 0.0 })),
//             (Variable::Bool(a), Variable::Float64(b)) => (if *a { 1.0 } else { 0.0 }).partial_cmp(b),
//             _ => None,
//         }
//     }
// }

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Expr {
    Var(Variable), // TODO: Ask Alex how'd you'd do this traditionally and what'd you'd call it
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    GreaterThan(Box<Variable>, Box<Variable>),
    LessThan(Box<Variable>, Box<Variable>),
    Equal(Box<Variable>, Box<Variable>),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Activation {
    Immediate(Expr),
    Hysteretic { enter: Expr, exit: Expr },
    // TODO: Implement some form of interrupt that can break out of any other mode, even if hysteretic and exit criteria not met
    // Requirement: Don't let Modes filibuster
    // TODO: Add hysteresis based on time, possible via a built in Variable of elapsed_time_active_s
}