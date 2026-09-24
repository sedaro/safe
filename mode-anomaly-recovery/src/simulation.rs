use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Serialize;

use crate::config::AllowedAction;
use crate::config::{
    ConstraintKind, SimulationScenario, SimulationScenarioRole, SimulationViabilityConfig,
};

#[derive(Debug, Clone)]
pub(crate) struct ScenarioRunRequest {
    pub(crate) scenario_id: String,
    pub(crate) evidence_revision: u64,
    pub(crate) horizon_days: f64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ScenarioRun {
    pub(crate) scenario_id: String,
    pub(crate) success: bool,
    pub(crate) timed_out: bool,
    pub(crate) evidence_revision: u64,
    pub(crate) horizon_days: f64,
    pub(crate) metrics: HashMap<String, UnitMetric>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UnitMetric {
    pub(crate) value: f64,
    pub(crate) units: String,
}

#[async_trait]
pub(crate) trait ScenarioRunner: Send + Sync {
    async fn run(&self, request: ScenarioRunRequest) -> Result<ScenarioRun>;
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PairedSimulation {
    pub(crate) thermal_benefit_verified: bool,
    pub(crate) baseline: ScenarioRun,
    pub(crate) recovery: ScenarioRun,
}

/// Validate the action-specific contract after selection. This deliberately
/// accepts power-only runs for command viability, but never labels them thermal.
pub(crate) async fn run_and_validate(
    runner: Arc<dyn ScenarioRunner>,
    baseline: &SimulationScenario,
    recovery: &SimulationScenario,
    viability: &SimulationViabilityConfig,
    action: AllowedAction,
    evidence_revision: u64,
    horizon_days: f64,
) -> Result<PairedSimulation> {
    if recovery.role != Some(SimulationScenarioRole::Recovery)
        || recovery.modeled_action != Some(action)
        || recovery.baseline_scenario_id.as_deref() != Some(baseline.id.as_str())
        || !recovery.has_action_binding()
    {
        bail!("selected action does not match the recovery scenario contract");
    }
    if baseline.role != Some(SimulationScenarioRole::Baseline)
        || baseline.duration_days != horizon_days
        || recovery.duration_days != horizon_days
        || baseline.state_bindings != recovery.state_bindings
    {
        bail!("baseline and recovery are not paired to the same frozen state and horizon");
    }
    for (metric_id, kind) in [
        (viability.final_soc_metric.as_str(), ConstraintKind::Minimum),
        (viability.min_soc_metric.as_str(), ConstraintKind::Minimum),
        (
            viability.max_soc_degradation_metric.as_str(),
            ConstraintKind::Maximum,
        ),
    ] {
        if !recovery
            .constraints
            .iter()
            .any(|constraint| constraint.metric_id == metric_id && constraint.kind == kind)
        {
            bail!(
                "recovery contract is missing constraint for '{}'",
                metric_id
            );
        }
    }

    let baseline_run = runner
        .run(ScenarioRunRequest {
            scenario_id: baseline.id.clone(),
            evidence_revision,
            horizon_days,
        })
        .await?;
    let recovery_run = runner
        .run(ScenarioRunRequest {
            scenario_id: recovery.id.clone(),
            evidence_revision,
            horizon_days,
        })
        .await?;

    validate_run(&baseline_run, &baseline.id, evidence_revision, horizon_days)?;
    validate_run(&recovery_run, &recovery.id, evidence_revision, horizon_days)?;
    let baseline_final = required_metric(&baseline_run, &viability.final_soc_metric)?;
    let baseline_min = required_metric(&baseline_run, &viability.min_soc_metric)?;
    let recovery_final = required_metric(&recovery_run, &viability.final_soc_metric)?;
    let recovery_min = required_metric(&recovery_run, &viability.min_soc_metric)?;
    if [baseline_min, recovery_final, recovery_min]
        .iter()
        .any(|m| m.units != baseline_final.units)
    {
        bail!("paired SOC metrics must use the same units");
    }
    let degradation = UnitMetric {
        value: (baseline_final.value - recovery_final.value)
            .max(baseline_min.value - recovery_min.value)
            .max(0.0),
        units: baseline_final.units.clone(),
    };
    validate_constraints(recovery, &recovery_run, &degradation, viability)?;
    Ok(PairedSimulation {
        thermal_benefit_verified: recovery.thermal
            && recovery
                .metrics
                .iter()
                .any(|metric| metric.quantity == viability.temperature_quantity),
        baseline: baseline_run,
        recovery: recovery_run,
    })
}

fn validate_run(run: &ScenarioRun, scenario_id: &str, revision: u64, horizon: f64) -> Result<()> {
    if run.scenario_id != scenario_id || !run.success || run.timed_out {
        bail!("scenario '{}' failed or timed out", scenario_id);
    }
    if run.evidence_revision != revision || run.horizon_days != horizon {
        bail!(
            "scenario '{}' is not tied to frozen evidence and horizon",
            scenario_id
        );
    }
    Ok(())
}

fn required_metric<'a>(run: &'a ScenarioRun, id: &str) -> Result<&'a UnitMetric> {
    let metric = run
        .metrics
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("required metric '{}' is unavailable", id))?;
    if metric.units.trim().is_empty() || !metric.value.is_finite() {
        bail!("required metric '{}' is not finite and unit-declared", id);
    }
    Ok(metric)
}

fn validate_constraints(
    scenario: &SimulationScenario,
    run: &ScenarioRun,
    degradation: &UnitMetric,
    viability: &SimulationViabilityConfig,
) -> Result<()> {
    for constraint in &scenario.constraints {
        let metric = if constraint.metric_id == viability.max_soc_degradation_metric {
            degradation
        } else {
            run.metrics.get(&constraint.metric_id).ok_or_else(|| {
                anyhow::anyhow!(
                    "constraint metric '{}' is unavailable",
                    constraint.metric_id
                )
            })?
        };
        if metric.units != constraint.units {
            bail!(
                "constraint metric '{}' has unexpected units",
                constraint.metric_id
            );
        }
        let passes = match constraint.kind {
            ConstraintKind::Minimum => metric.value >= constraint.value,
            ConstraintKind::Maximum => metric.value <= constraint.value,
        };
        if !passes {
            bail!("constraint '{}' failed", constraint.metric_id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SimulationConstraint, SimulationMetric};
    use std::sync::Mutex;

    const FINAL_SOC: &str = "final_state_of_charge";
    const MIN_SOC: &str = "minimum_state_of_charge";
    const MAX_SOC_DEGRADATION: &str = "maximum_state_of_charge_degradation";

    struct FakeRunner {
        runs: Mutex<Vec<Result<ScenarioRun, String>>>,
    }

    #[async_trait]
    impl ScenarioRunner for FakeRunner {
        async fn run(&self, _request: ScenarioRunRequest) -> Result<ScenarioRun> {
            self.runs
                .lock()
                .unwrap()
                .remove(0)
                .map_err(anyhow::Error::msg)
        }
    }

    fn scenario(id: &str, role: SimulationScenarioRole) -> SimulationScenario {
        SimulationScenario {
            id: id.into(),
            description: id.into(),
            applicable_rule_ids: vec!["rule".into()],
            allowed_actions: vec![AllowedAction::PointNadir],
            baseline_scenario_id: (role == SimulationScenarioRole::Recovery)
                .then(|| "baseline".into()),
            modeled_action: (role == SimulationScenarioRole::Recovery)
                .then_some(AllowedAction::PointNadir),
            role: Some(role),
            command_schedule_binding: (role == SimulationScenarioRole::Recovery)
                .then(|| "schedule".into()),
            compute_power_binding: None,
            state_bindings: vec![],
            constraints: vec![
                SimulationConstraint {
                    metric_id: FINAL_SOC.into(),
                    kind: ConstraintKind::Minimum,
                    value: 0.6,
                    units: "fraction".into(),
                },
                SimulationConstraint {
                    metric_id: MIN_SOC.into(),
                    kind: ConstraintKind::Minimum,
                    value: 0.4,
                    units: "fraction".into(),
                },
                SimulationConstraint {
                    metric_id: MAX_SOC_DEGRADATION.into(),
                    kind: ConstraintKind::Maximum,
                    value: 0.2,
                    units: "fraction".into(),
                },
            ],
            thermal: false,
            duration_days: 1.0,
            patches: vec![],
            parameters: vec![],
            metrics: vec![
                SimulationMetric {
                    id: FINAL_SOC.into(),
                    quantity: FINAL_SOC.into(),
                    units: "fraction".into(),
                    target_file: "generic".into(),
                    field: "final".into(),
                    aggregation: crate::config::MetricAggregation::Last,
                },
                SimulationMetric {
                    id: MIN_SOC.into(),
                    quantity: MIN_SOC.into(),
                    units: "fraction".into(),
                    target_file: "generic".into(),
                    field: "minimum".into(),
                    aggregation: crate::config::MetricAggregation::Min,
                },
            ],
        }
    }

    fn run(id: &str, value: f64) -> ScenarioRun {
        ScenarioRun {
            scenario_id: id.into(),
            success: true,
            timed_out: false,
            evidence_revision: 7,
            horizon_days: 1.0,
            metrics: HashMap::from([
                (
                    FINAL_SOC.into(),
                    UnitMetric {
                        value,
                        units: "fraction".into(),
                    },
                ),
                (
                    MIN_SOC.into(),
                    UnitMetric {
                        value: value - 0.1,
                        units: "fraction".into(),
                    },
                ),
            ]),
        }
    }

    #[tokio::test]
    async fn paired_success_is_action_specific_and_power_only_is_not_thermal() {
        let result = run_and_validate(
            Arc::new(FakeRunner {
                runs: Mutex::new(vec![Ok(run("baseline", 0.8)), Ok(run("recovery", 0.75))]),
            }),
            &scenario("baseline", SimulationScenarioRole::Baseline),
            &scenario("recovery", SimulationScenarioRole::Recovery),
            &SimulationViabilityConfig::default(),
            AllowedAction::PointNadir,
            7,
            1.0,
        )
        .await
        .unwrap();
        assert!(!result.thermal_benefit_verified);
    }

    #[tokio::test]
    async fn failures_timeout_missing_metric_and_constraint_block() {
        let baseline = scenario("baseline", SimulationScenarioRole::Baseline);
        let recovery = scenario("recovery", SimulationScenarioRole::Recovery);
        for runs in [
            vec![Err("failed".into()), Ok(run("recovery", 0.8))],
            vec![
                Ok(run("baseline", 0.8)),
                Ok(ScenarioRun {
                    metrics: HashMap::new(),
                    ..run("recovery", 0.8)
                }),
            ],
        ] {
            assert!(
                run_and_validate(
                    Arc::new(FakeRunner {
                        runs: Mutex::new(runs)
                    }),
                    &baseline,
                    &recovery,
                    &SimulationViabilityConfig::default(),
                    AllowedAction::PointNadir,
                    7,
                    1.0
                )
                .await
                .is_err()
            );
        }
        let mut low = run("recovery", 0.5);
        low.timed_out = true;
        assert!(
            run_and_validate(
                Arc::new(FakeRunner {
                    runs: Mutex::new(vec![Ok(run("baseline", 0.8)), Ok(low)])
                }),
                &baseline,
                &recovery,
                &SimulationViabilityConfig::default(),
                AllowedAction::PointNadir,
                7,
                1.0
            )
            .await
            .is_err()
        );
        assert!(
            run_and_validate(
                Arc::new(FakeRunner {
                    runs: Mutex::new(vec![Ok(run("baseline", 0.8)), Ok(run("recovery", 0.5))])
                }),
                &baseline,
                &recovery,
                &SimulationViabilityConfig::default(),
                AllowedAction::PointNadir,
                7,
                1.0
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn inconsistent_soc_units_and_nonfinite_metrics_block() {
        for units in ["percent", "fraction"] {
            let mut baseline = run("baseline", 0.8);
            let metric = baseline.metrics.get_mut(MIN_SOC).unwrap();
            metric.units = units.into();
            if units == "fraction" {
                metric.value = f64::NAN;
            }
            assert!(
                run_and_validate(
                    Arc::new(FakeRunner {
                        runs: Mutex::new(vec![Ok(baseline), Ok(run("recovery", 0.75))])
                    }),
                    &scenario("baseline", SimulationScenarioRole::Baseline),
                    &scenario("recovery", SimulationScenarioRole::Recovery),
                    &SimulationViabilityConfig::default(),
                    AllowedAction::PointNadir,
                    7,
                    1.0,
                )
                .await
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn action_mismatch_and_stale_revision_block() {
        let baseline = scenario("baseline", SimulationScenarioRole::Baseline);
        let recovery = scenario("recovery", SimulationScenarioRole::Recovery);
        assert!(
            run_and_validate(
                Arc::new(FakeRunner {
                    runs: Mutex::new(vec![])
                }),
                &baseline,
                &recovery,
                &SimulationViabilityConfig::default(),
                AllowedAction::PointSunYaw,
                7,
                1.0
            )
            .await
            .is_err()
        );
        let mut stale = run("baseline", 0.8);
        stale.evidence_revision = 8;
        assert!(
            run_and_validate(
                Arc::new(FakeRunner {
                    runs: Mutex::new(vec![Ok(stale), Ok(run("recovery", 0.75))])
                }),
                &baseline,
                &recovery,
                &SimulationViabilityConfig::default(),
                AllowedAction::PointNadir,
                7,
                1.0
            )
            .await
            .is_err()
        );
    }
}
