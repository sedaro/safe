use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

#[derive(Debug)]
pub enum Error {
    UndefinedVariable(String),
    UndefinedTelemetry(String),
    TypeMismatch,
    NotComparable,
    NotBoolean,
}

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
pub enum Variable {
    String(Value<String>),
    Float64(Value<f64>),
    Bool(Value<bool>),
}
impl Variable {
    fn resolve(&self, ctx: &impl Resolvable) -> Result<ResolvedValue, Error> {
        match self {
            Variable::String(v) => v.resolve_string(ctx).map(ResolvedValue::String), // FIXME: Make same?
            Variable::Float64(v) => v.resolve_f64(ctx).map(ResolvedValue::Float),
            Variable::Bool(v) => v.resolve_bool(ctx).map(ResolvedValue::Bool),
        }
    }
}

pub trait Resolvable {
    fn get_variable(&self, name: &str) -> Option<Variable>;
    fn get_telemetry(&self, name: &str) -> Option<Variable>;
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Value<V> {
    Literal(V),
    VariableRef(String),
    TelemetryRef(String),
}
impl Value<String> { // FIXME: Consolidate?
    fn resolve_string(&self, ctx: &impl Resolvable) -> Result<String, Error> {
        match self {
            Value::Literal(s) => Ok(s.clone()),
            Value::VariableRef(name) => match ctx.get_variable(name) {
                Some(Variable::String(v)) => v.resolve_string(ctx),
                Some(_) => Err(Error::TypeMismatch),
                None => Err(Error::UndefinedVariable(name.clone())),
            },
            Value::TelemetryRef(name) => match ctx.get_telemetry(name) {
                Some(Variable::String(v)) => v.resolve_string(ctx),
                Some(_) => Err(Error::TypeMismatch),
                None => Err(Error::UndefinedTelemetry(name.clone())),
            },
        }
    }
}
impl Value<f64> {
    fn resolve_f64(&self, ctx: &impl Resolvable) -> Result<f64, Error> {
        match self {
            Value::Literal(f) => Ok(*f),
            Value::VariableRef(name) => match ctx.get_variable(name) {
                Some(Variable::Float64(v)) => v.resolve_f64(ctx),
                Some(_) => Err(Error::TypeMismatch),
                None => Err(Error::UndefinedVariable(name.clone())),
            },
            Value::TelemetryRef(name) => match ctx.get_telemetry(name) {
                Some(Variable::Float64(v)) => v.resolve_f64(ctx),
                Some(_) => Err(Error::TypeMismatch),
                None => Err(Error::UndefinedTelemetry(name.clone())),
            },
        }
    }
}
impl Value<bool> {
    fn resolve_bool(&self, ctx: &impl Resolvable) -> Result<bool, Error> {
        match self {
            Value::Literal(b) => Ok(*b),
            Value::VariableRef(name) => match ctx.get_variable(name) {
                Some(Variable::Bool(v)) => v.resolve_bool(ctx),
                Some(_) => Err(Error::TypeMismatch),
                None => Err(Error::UndefinedVariable(name.clone())),
            },
            Value::TelemetryRef(name) => match ctx.get_telemetry(name) {
                Some(Variable::Bool(v)) => v.resolve_bool(ctx),
                Some(_) => Err(Error::TypeMismatch),
                None => Err(Error::UndefinedTelemetry(name.clone())),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ResolvedValue {
    String(String),
    Float(f64),
    Bool(bool),
}

impl ResolvedValue {
    fn as_bool(&self) -> Result<bool, Error> {
        match self {
            ResolvedValue::Bool(b) => Ok(*b),
            _ => Err(Error::NotBoolean),
        }
    }
    fn try_cmp(&self, other: &Self) -> Result<Option<Ordering>, Error> {
        match (self, other) {
            (ResolvedValue::Float(a), ResolvedValue::Float(b)) => Ok(a.partial_cmp(b)),
            (ResolvedValue::String(a), ResolvedValue::String(b)) => Ok(Some(a.cmp(b))),
            (ResolvedValue::Bool(a), ResolvedValue::Bool(b)) => Ok(Some(a.cmp(b))),
            _ => Err(Error::TypeMismatch),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Expr {
    Term(Variable),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    GreaterThan(Box<Expr>, Box<Expr>),
    LessThan(Box<Expr>, Box<Expr>),
    Equal(Box<Expr>, Box<Expr>),
}
impl Expr {
    // TODO: Write comprehensive unit tests
    // TODO: Make this not panic
    pub fn eval(&self, ctx: &impl Resolvable) -> Result<bool, Error> {
        match self {
            Expr::Term(var) => var.resolve(ctx)?.as_bool(),
            Expr::And(exprs) => {
                for expr in exprs {
                    if !expr.eval(ctx)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Expr::Or(exprs) => {
                for expr in exprs {
                    if expr.eval(ctx)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Expr::Not(expr) => Ok(!expr.eval(ctx)?),
            Expr::GreaterThan(l, r) => {
                let (lv, rv) = (Self::eval_value(l, ctx)?, Self::eval_value(r, ctx)?);
                lv.try_cmp(&rv)?.ok_or(Error::NotComparable).map(|o| o.is_gt())
            }
            Expr::LessThan(l, r) => {
                let (lv, rv) = (Self::eval_value(l, ctx)?, Self::eval_value(r, ctx)?);
                lv.try_cmp(&rv)?.ok_or(Error::NotComparable).map(|o| o.is_lt())
            }
            Expr::Equal(l, r) => {
                Ok(Self::eval_value(l, ctx)? == Self::eval_value(r, ctx)?)
            }
        }
    }
    fn eval_value(expr: &Expr, ctx: &impl Resolvable) -> Result<ResolvedValue, Error> {
        match expr {
            Expr::Term(var) => var.resolve(ctx),
            other => other.eval(ctx).map(ResolvedValue::Bool),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Activation {
    Immediate(Expr),
    Hysteretic { enter: Expr, exit: Expr },
    // TODO: Implement some form of interrupt that can break out of any other mode, even if hysteretic and exit criteria not met
    // Requirement: Don't let Modes filibuster
    // TODO: Add hysteresis based on time, possible via a built in Variable of elapsed_time_active_s
}
