use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

#[derive(Debug)]
pub enum Error {
    UndefinedVariable(String),
    UndefinedTelemetry(String),
    TypeMismatch,
    NotComparable,
    UnresolvedVariable,
}

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

pub trait Resolvable {
    fn get_variable(&self, name: &str) -> Option<Variable>;
    fn get_telemetry_point(&self, name: &str) -> Option<Variable>;
    fn get_telemetry_points(&self, name: &str, points: usize) -> Vec<Variable>;
    // fn get_telemetry_points_since(&self, name: &str, points: i32) -> Option<<Variable>>;
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, PartialOrd, Eq, Ord)]
pub enum Value<V> {
    Literal(V),
    VariableRef(String),
    TelemetryRef(String),
    AverageTelemetryRef{ name: String, points: usize, },
    // AverageSinceTelemetryRef{ name: String, todo: f64, },
}
impl Value<String> {
    fn resolve_string(&self, ctx: &impl Resolvable) -> Result<String, Error> {
        match self {
            Value::Literal(s) => Ok(s.clone()),
            Value::VariableRef(name) => match ctx.get_variable(name) {
                Some(Variable::String(v)) => v.resolve_string(ctx),
                Some(_) => Err(Error::TypeMismatch),
                None => Err(Error::UndefinedVariable(name.clone())),
            },
            Value::TelemetryRef(name) => match ctx.get_telemetry_point(name) {
                Some(Variable::String(v)) => v.resolve_string(ctx),
                Some(_) => Err(Error::TypeMismatch),
                None => Err(Error::UndefinedTelemetry(name.clone())),
            },
            Value::AverageTelemetryRef{name, points} => {
                let points = ctx.get_telemetry_points(name, *points);
                if points.is_empty() {
                  return Err(Error::UndefinedTelemetry(name.clone()));
                }
                unimplemented!()
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
            Value::TelemetryRef(name) => match ctx.get_telemetry_point(name) {
                Some(Variable::Float64(v)) => v.resolve_f64(ctx),
                Some(_) => Err(Error::TypeMismatch),
                None => Err(Error::UndefinedTelemetry(name.clone())),
            },
            Value::AverageTelemetryRef{name, points} => {
                let points = ctx.get_telemetry_points(name, *points);
                if points.is_empty() {
                  return Err(Error::UndefinedTelemetry(name.clone()));
                }
                let mut sum = 0.0;
                for point in &points {
                  match point {
                    Variable::Float64(v) => {
                      sum += v.resolve_f64(ctx)?;
                    },
                    _ => return Err(Error::TypeMismatch),
                  }
                }
                Ok(sum / (points.len() as f64))
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
            Value::TelemetryRef(name) => match ctx.get_telemetry_point(name) {
                Some(Variable::Bool(v)) => v.resolve_bool(ctx),
                Some(_) => Err(Error::TypeMismatch),
                None => Err(Error::UndefinedTelemetry(name.clone())),
            },
            Value::AverageTelemetryRef{name, points} => {
                let points = ctx.get_telemetry_points(name, *points);
                if points.is_empty() {
                  return Err(Error::UndefinedTelemetry(name.clone()));
                }
                let mut sum = 0.0;
                for point in &points {
                  match point {
                    Variable::Bool(v) => {
                      sum += if v.resolve_bool(ctx)? { 1.0 } else { 0.0 };
                    },
                    _ => return Err(Error::TypeMismatch),
                  }
                }
                Ok((sum / (points.len() as f64)) >= 0.5)
            },
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Variable {
    String(Value<String>),
    Float64(Value<f64>),
    Bool(Value<bool>),
}
impl Variable {
    fn as_bool(&self) -> Result<bool, Error> {
        match self {
            Variable::Bool(Value::Literal(v)) => Ok(*v),
            Variable::Bool(_) => Err(Error::UnresolvedVariable),
            Variable::Float64(Value::Literal(v)) => Ok(*v != 0.0),
            Variable::Float64(_) => Err(Error::UnresolvedVariable),
            Variable::String(Value::Literal(s)) => Ok(!s.is_empty()), // TODO: Revisit truthiness of strings if we want to change how they are interpreted   
            Variable::String(_) => Err(Error::UnresolvedVariable),
        }
    }
    fn try_cmp(&self, other: &Self) -> Result<Option<Ordering>, Error> {
        match (self, other) {
            (Variable::String(a), Variable::String(b)) => Ok(Some(a.cmp(b))),
            (Variable::Float64(a), Variable::Float64(b)) => Ok(a.partial_cmp(b)),
            (Variable::Bool(a), Variable::Bool(b)) => Ok(Some(a.cmp(b))),
            (Variable::Float64(Value::Literal(a)), Variable::Bool(Value::Literal(b))) => Ok((*a != 0.0).partial_cmp(b)),           
            (Variable::Bool(Value::Literal(a)), Variable::Float64(Value::Literal(b))) => Ok(a.partial_cmp(&(*b != 0.0))),
            _ => Err(Error::TypeMismatch),
        }
    }
    fn resolve(&self, ctx: &impl Resolvable) -> Result<Variable, Error> {
        match self {
            Variable::String(v) => v.resolve_string(ctx).map(|v| Variable::String(Value::Literal(v))),
            Variable::Float64(v) => v.resolve_f64(ctx).map(|v| Variable::Float64(Value::Literal(v))),
            Variable::Bool(v) => v.resolve_bool(ctx).map(|v| Variable::Bool(Value::Literal(v))),
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
    pub fn and(exprs: Vec<Expr>) -> Self {
        Expr::And(exprs)
    }
    pub fn or(exprs: Vec<Expr>) -> Self {
        Expr::Or(exprs)
    }
    pub fn not(expr: Expr) -> Self {
        Expr::Not(Box::new(expr))
    }
    pub fn greater_than(left: Expr, right: Expr) -> Self {
        Expr::GreaterThan(Box::new(left), Box::new(right))
    }
    pub fn less_than(left: Expr, right: Expr) -> Self {
        Expr::LessThan(Box::new(left), Box::new(right))
    }
    pub fn equal(left: Expr, right: Expr) -> Self {
        Expr::Equal(Box::new(left), Box::new(right))
    }

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
                let (lv, rv) = (Self::eval_value(l, ctx)?, Self::eval_value(r, ctx)?);
                lv.try_cmp(&rv)?.ok_or(Error::NotComparable).map(|o| o.is_eq())
            }
        }
    }
    fn eval_value(expr: &Expr, ctx: &impl Resolvable) -> Result<Variable, Error> {
        match expr {
            Expr::Term(var) => var.resolve(ctx),
            other => other.eval(ctx).map(|b| Variable::Bool(Value::Literal(b))),
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
