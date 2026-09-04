use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, bail};
use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;
use safe_sim::{EdsPatch, ProbabilityDistribution};

use crate::gatekeeper_types::{
    MonteCarloBounds, MonteCarloConfig, MonteCarloOperation, MonteCarloVariation,
};

/// A complete patch set for one deterministic Monte Carlo sample.
#[derive(Clone, Debug)]
pub(crate) struct MonteCarloCase {
    pub id: String,
    pub seed: u64,
    pub values: BTreeMap<String, f64>,
    pub patches: Vec<EdsPatch>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct PatchKey {
    agent_id: String,
    engine: String,
    field: String,
}

impl PatchKey {
    fn from_patch(patch: &EdsPatch) -> Self {
        Self {
            agent_id: patch.agent_id.clone(),
            engine: patch.engine.clone(),
            field: patch.field.clone(),
        }
    }

    fn from_variation(variation: &MonteCarloVariation) -> Self {
        Self {
            agent_id: variation.target.agent_id.clone(),
            engine: variation.target.engine.clone(),
            field: variation.target.field.clone(),
        }
    }

    fn display(&self) -> String {
        format!("{}/{}/{}", self.agent_id, self.engine, self.field)
    }
}

/// Rejects ambiguous adapter output before EDS decides which duplicate patch wins.
pub(crate) fn validate_baseline_patches(patches: &[EdsPatch]) -> Result<()> {
    let mut targets = HashSet::new();
    for patch in patches {
        let key = PatchKey::from_patch(patch);
        if !targets.insert(key.clone()) {
            bail!(
                "simulation input adapter returned duplicate patch target '{}'",
                key.display()
            );
        }
    }
    Ok(())
}

/// Generates scalar cases around the baseline values materialized by the adapter.
pub(crate) fn generate_cases(
    config: &MonteCarloConfig,
    baseline_patches: &[EdsPatch],
) -> Result<Vec<MonteCarloCase>> {
    validate_config(config)?;
    validate_baseline_patches(baseline_patches)?;

    let baseline_indices: HashMap<_, _> = baseline_patches
        .iter()
        .enumerate()
        .map(|(index, patch)| (PatchKey::from_patch(patch), index))
        .collect();
    validate_variation_targets(config, baseline_patches, &baseline_indices)?;

    let mut seed_rng = ChaCha8Rng::seed_from_u64(config.seed);
    let mut cases = Vec::with_capacity(config.samples);
    for index in 0..config.samples {
        let case_seed = seed_rng.next_u64();
        let mut case_rng = ChaCha8Rng::seed_from_u64(case_seed);
        let mut patches = baseline_patches.to_vec();
        let mut values = BTreeMap::new();

        for variation in &config.variations {
            let key = PatchKey::from_variation(variation);
            let baseline_index = baseline_indices.get(&key).copied();
            let baseline = baseline_index
                .map(|patch_index| parse_baseline(&patches[patch_index], variation))
                .transpose()?;
            let final_value = sample_bounded_value(
                variation,
                baseline,
                config.max_resample_attempts,
                &mut case_rng,
            )?;

            if let Some(patch_index) = baseline_index {
                patches[patch_index].value = final_value.to_string();
            } else {
                patches.push(variation.target.patch(final_value));
            }
            values.insert(variation.name.clone(), final_value);
        }

        cases.push(MonteCarloCase {
            id: format!("sample-{index:04}"),
            seed: case_seed,
            values,
            patches,
        });
    }
    Ok(cases)
}

fn validate_config(config: &MonteCarloConfig) -> Result<()> {
    if config.samples == 0 {
        bail!("gatekeeper Monte Carlo samples must be greater than zero");
    }
    if config.variations.is_empty() {
        bail!("gatekeeper Monte Carlo requires at least one variation");
    }
    if config.max_resample_attempts == 0 {
        bail!("gatekeeper Monte Carlo max_resample_attempts must be greater than zero");
    }
    if !config.minimum_pass_fraction.is_finite()
        || config.minimum_pass_fraction <= 0.0
        || config.minimum_pass_fraction > 1.0
    {
        bail!("gatekeeper Monte Carlo minimum_pass_fraction must be in (0, 1]");
    }

    let mut names = HashSet::new();
    let mut targets = HashSet::new();
    for variation in &config.variations {
        if variation.name.trim().is_empty() {
            bail!("gatekeeper Monte Carlo variation names must not be empty");
        }
        if !names.insert(variation.name.as_str()) {
            bail!(
                "duplicate gatekeeper Monte Carlo variation name '{}'",
                variation.name
            );
        }
        if variation.target.agent_id.trim().is_empty()
            || variation.target.engine.trim().is_empty()
            || variation.target.field.trim().is_empty()
            || variation.target.type_.trim().is_empty()
        {
            bail!(
                "Monte Carlo variation '{}' has an incomplete patch target",
                variation.name
            );
        }
        let target = PatchKey::from_variation(variation);
        if !targets.insert(target.clone()) {
            bail!(
                "multiple Monte Carlo variations target patch '{}'",
                target.display()
            );
        }
        validate_bounds(variation)?;
    }
    Ok(())
}

fn validate_bounds(variation: &MonteCarloVariation) -> Result<()> {
    let Some(bounds) = variation.bounds else {
        return Ok(());
    };
    if bounds.min.is_some_and(|value| !value.is_finite())
        || bounds.max.is_some_and(|value| !value.is_finite())
    {
        bail!(
            "Monte Carlo variation '{}' bounds must be finite",
            variation.name
        );
    }
    if matches!((bounds.min, bounds.max), (Some(min), Some(max)) if min > max) {
        bail!(
            "Monte Carlo variation '{}' bounds require min <= max",
            variation.name
        );
    }
    Ok(())
}

fn validate_variation_targets(
    config: &MonteCarloConfig,
    baseline_patches: &[EdsPatch],
    baseline_indices: &HashMap<PatchKey, usize>,
) -> Result<()> {
    for variation in &config.variations {
        let key = PatchKey::from_variation(variation);
        let Some(index) = baseline_indices.get(&key).copied() else {
            if !matches!(variation.operation, MonteCarloOperation::Replace) {
                bail!(
                    "Monte Carlo variation '{}' uses {:?} but adapter patch '{}' is absent",
                    variation.name,
                    variation.operation,
                    key.display()
                );
            }
            continue;
        };

        let patch = &baseline_patches[index];
        if patch.type_ != variation.target.type_ {
            bail!(
                "Monte Carlo variation '{}' type '{}' conflicts with adapter type '{}' for patch '{}'",
                variation.name,
                variation.target.type_,
                patch.type_,
                key.display()
            );
        }
        if !matches!(variation.operation, MonteCarloOperation::Replace) {
            parse_baseline(patch, variation)?;
        }
    }
    Ok(())
}

fn parse_baseline(patch: &EdsPatch, variation: &MonteCarloVariation) -> Result<f64> {
    let value = patch.value.parse::<f64>().with_context(|| {
        format!(
            "Monte Carlo variation '{}' requires numeric baseline value '{}'",
            variation.name, patch.value
        )
    })?;
    if !value.is_finite() {
        bail!(
            "Monte Carlo variation '{}' baseline must be finite",
            variation.name
        );
    }
    Ok(value)
}

fn sample_bounded_value(
    variation: &MonteCarloVariation,
    baseline: Option<f64>,
    max_resample_attempts: usize,
    rng: &mut ChaCha8Rng,
) -> Result<f64> {
    let distribution = ProbabilityDistribution::from(&variation.distribution);
    for _ in 0..max_resample_attempts {
        let sampled = distribution.sample(rng)?;
        let value = match variation.operation {
            MonteCarloOperation::Replace => sampled,
            MonteCarloOperation::Add => {
                baseline.context("add operation requires a baseline")? + sampled
            }
            MonteCarloOperation::Multiply => {
                baseline.context("multiply operation requires a baseline")? * sampled
            }
        };
        if value.is_finite() && within_bounds(value, variation.bounds) {
            return Ok(value);
        }
    }
    bail!(
        "Monte Carlo variation '{}' could not produce an in-bounds finite value after {} attempts",
        variation.name,
        max_resample_attempts
    )
}

fn within_bounds(value: f64, bounds: Option<MonteCarloBounds>) -> bool {
    bounds.is_none_or(|bounds| {
        bounds.min.is_none_or(|min| value >= min) && bounds.max.is_none_or(|max| value <= max)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gatekeeper_types::{MonteCarloDistribution, MonteCarloOperation};
    use safe_sim::EdsPatchTarget;

    fn config(operation: MonteCarloOperation) -> MonteCarloConfig {
        MonteCarloConfig {
            samples: 2,
            seed: 42,
            minimum_pass_fraction: 1.0,
            max_resample_attempts: 1_000,
            variations: vec![MonteCarloVariation {
                name: "soc".to_string(),
                target: EdsPatchTarget::new("agent", "power", "battery.soc", "f64"),
                operation,
                distribution: MonteCarloDistribution::Discrete { values: vec![0.1] },
                bounds: None,
            }],
        }
    }

    fn baseline(value: &str) -> Vec<EdsPatch> {
        vec![EdsPatch::new("agent", "power", "battery.soc", "f64", value)]
    }

    #[test]
    fn add_uses_adapter_value_as_baseline() {
        let cases = generate_cases(&config(MonteCarloOperation::Add), &baseline("0.789")).unwrap();
        assert_eq!(cases[0].values["soc"], 0.889);
        assert_eq!(cases[0].patches[0].value, "0.889");
    }

    #[test]
    fn multiply_uses_adapter_value_as_baseline() {
        let cases =
            generate_cases(&config(MonteCarloOperation::Multiply), &baseline("0.5")).unwrap();
        assert_eq!(cases[0].values["soc"], 0.05);
        assert_eq!(cases[0].patches[0].value, "0.05");
    }

    #[test]
    fn absent_replace_target_creates_complete_patch() {
        let cases = generate_cases(&config(MonteCarloOperation::Replace), &[]).unwrap();
        assert_eq!(cases[0].patches, baseline("0.1"));
    }

    #[test]
    fn absent_relative_target_is_rejected() {
        let error = generate_cases(&config(MonteCarloOperation::Multiply), &[]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("adapter patch 'agent/power/battery.soc' is absent")
        );
    }

    #[test]
    fn matching_field_with_different_type_is_rejected() {
        let patches = vec![EdsPatch::new(
            "agent",
            "power",
            "battery.soc",
            "float",
            "0.789",
        )];
        let error = generate_cases(&config(MonteCarloOperation::Add), &patches).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("conflicts with adapter type 'float'")
        );
    }

    #[test]
    fn bounded_values_are_resampled_deterministically() {
        let mut bounded = config(MonteCarloOperation::Add);
        bounded.variations[0].distribution = MonteCarloDistribution::Discrete {
            values: vec![-1.0, 0.01],
        };
        bounded.variations[0].bounds = Some(MonteCarloBounds {
            min: Some(0.0),
            max: Some(1.0),
        });
        let first = generate_cases(&bounded, &baseline("0.789")).unwrap();
        let second = generate_cases(&bounded, &baseline("0.789")).unwrap();
        assert_eq!(first[0].patches, second[0].patches);
        assert!(first.iter().all(|case| {
            let value = case.patches[0].value.parse::<f64>().unwrap();
            (0.0..=1.0).contains(&value)
        }));
    }

    #[test]
    fn duplicate_logical_baseline_target_is_rejected() {
        let patches = vec![
            EdsPatch::new("agent", "power", "battery.soc", "f64", "0.7"),
            EdsPatch::new("agent", "power", "battery.soc", "float", "0.8"),
        ];
        let error = validate_baseline_patches(&patches).unwrap_err();
        assert!(error.to_string().contains("duplicate patch target"));
    }

    #[test]
    fn zero_resample_attempts_is_rejected() {
        let mut invalid = config(MonteCarloOperation::Replace);
        invalid.max_resample_attempts = 0;
        let error = generate_cases(&invalid, &[]).unwrap_err();
        assert!(error.to_string().contains("max_resample_attempts"));
    }
}
