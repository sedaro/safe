use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use safe_llm_adapter::AdapterSelection;
use serde::{Deserialize, Serialize};
use serde_json::json;

const MAX_OUTPUT_TOKENS: u32 = 2_048;
const DEFAULT_CONTEXT_WINDOW_TOKENS: u32 = 2_048;
const DEFAULT_CONTEXT_SAFETY_MARGIN_TOKENS: u32 = 256;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AllowedAction {
    PointSunYaw,
    PointNadir,
    ThrusterOff,
    CaptureImage,
    Noop,
}

impl AllowedAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::PointSunYaw => "point_sun_yaw",
            Self::PointNadir => "point_nadir",
            Self::ThrusterOff => "thruster_off",
            Self::CaptureImage => "capture_image",
            Self::Noop => "noop",
        }
    }

    pub(crate) fn is_recommendable(self) -> bool {
        matches!(
            self,
            Self::PointSunYaw | Self::PointNadir | Self::ThrusterOff
        )
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActionDefinition {
    pub(crate) id: AllowedAction,
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) preconditions: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AnomalySeverity {
    Info,
    Low,
    #[default]
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NominalRuleKind {
    NumberRange,
    Enum,
    Boolean,
    Required,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NominalRule {
    pub(crate) id: String,
    pub(crate) path: String,
    pub(crate) kind: NominalRuleKind,
    #[serde(default)]
    pub(crate) min: Option<f64>,
    #[serde(default)]
    pub(crate) max: Option<f64>,
    #[serde(default)]
    pub(crate) allowed: Vec<String>,
    #[serde(default)]
    pub(crate) expected: Option<bool>,
    #[serde(default = "default_min_consecutive_samples")]
    pub(crate) min_consecutive_samples: usize,
    #[serde(default)]
    pub(crate) severity: AnomalySeverity,
    #[serde(default)]
    pub(crate) eligible_actions: Vec<AllowedAction>,
}

impl NominalRule {
    pub(crate) fn expectation_description(&self) -> String {
        match self.kind {
            NominalRuleKind::NumberRange => match (self.min, self.max) {
                (Some(min), Some(max)) => format!("between {min} and {max}"),
                (Some(min), None) => format!("at least {min}"),
                (None, Some(max)) => format!("at most {max}"),
                (None, None) => "a configured numeric range".to_string(),
            },
            NominalRuleKind::Enum => format!("one of {}", self.allowed.join(", ")),
            NominalRuleKind::Boolean => match self.expected {
                Some(expected) => expected.to_string(),
                None => "a configured boolean value".to_string(),
            },
            NominalRuleKind::Required => "present and non-null".to_string(),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            bail!("nominal rule id must not be empty");
        }
        validate_path(&self.path).map_err(|e| anyhow!("rule '{}': {e}", self.id))?;
        if self.min_consecutive_samples == 0 {
            bail!(
                "rule '{}': min_consecutive_samples must be greater than zero",
                self.id
            );
        }

        let unique_actions = self.eligible_actions.iter().collect::<HashSet<_>>();
        if unique_actions.len() != self.eligible_actions.len() {
            bail!("rule '{}': eligible actions must be unique", self.id);
        }

        match self.kind {
            NominalRuleKind::NumberRange => {
                let min = self.min;
                let max = self.max;
                if min.is_none() && max.is_none() {
                    bail!("rule '{}': number_range needs min and/or max", self.id);
                }
                if min.is_some_and(|value| !value.is_finite())
                    || max.is_some_and(|value| !value.is_finite())
                {
                    bail!("rule '{}': numeric limits must be finite", self.id);
                }
                if let (Some(min), Some(max)) = (min, max)
                    && min > max
                {
                    bail!("rule '{}': min must not exceed max", self.id);
                }
                if !self.allowed.is_empty() || self.expected.is_some() {
                    bail!(
                        "rule '{}': number_range cannot define allowed or expected",
                        self.id
                    );
                }
            }
            NominalRuleKind::Enum => {
                if self.allowed.is_empty() {
                    bail!("rule '{}': enum needs at least one allowed value", self.id);
                }
                if self.allowed.iter().any(|value| value.is_empty()) {
                    bail!("rule '{}': enum values must not be empty", self.id);
                }
                let unique = self.allowed.iter().collect::<HashSet<_>>();
                if unique.len() != self.allowed.len() {
                    bail!("rule '{}': enum values must be unique", self.id);
                }
                if self.min.is_some() || self.max.is_some() || self.expected.is_some() {
                    bail!(
                        "rule '{}': enum cannot define min, max, or expected",
                        self.id
                    );
                }
            }
            NominalRuleKind::Boolean => {
                if self.expected.is_none() {
                    bail!("rule '{}': boolean needs expected", self.id);
                }
                if self.min.is_some() || self.max.is_some() || !self.allowed.is_empty() {
                    bail!(
                        "rule '{}': boolean cannot define min, max, or allowed",
                        self.id
                    );
                }
            }
            NominalRuleKind::Required => {
                if self.min.is_some()
                    || self.max.is_some()
                    || !self.allowed.is_empty()
                    || self.expected.is_some()
                {
                    bail!(
                        "rule '{}': required cannot define min, max, allowed, or expected",
                        self.id
                    );
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NominalProfile {
    pub(crate) id: String,
    pub(crate) source: String,
    pub(crate) rules: Vec<NominalRule>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SimulationPatchBinding {
    pub(crate) agent_id: String,
    pub(crate) engine: String,
    pub(crate) field: String,
    #[serde(rename = "type")]
    pub(crate) type_: String,
    #[serde(default)]
    pub(crate) value: Option<f64>,
    #[serde(default)]
    pub(crate) telemetry_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SimulationParameter {
    pub(crate) id: String,
    pub(crate) patch_index: usize,
    pub(crate) min: f64,
    pub(crate) max: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MetricAggregation {
    Last,
    Min,
    Max,
    Mean,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SimulationMetric {
    pub(crate) id: String,
    pub(crate) quantity: String,
    pub(crate) units: String,
    pub(crate) target_file: String,
    pub(crate) field: String,
    pub(crate) aggregation: MetricAggregation,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SimulationScenarioRole {
    Baseline,
    Recovery,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SimulationStateBinding {
    pub(crate) id: String,
    pub(crate) source: String,
    pub(crate) path: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConstraintKind {
    Minimum,
    Maximum,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SimulationConstraint {
    pub(crate) metric_id: String,
    pub(crate) kind: ConstraintKind,
    pub(crate) value: f64,
    pub(crate) units: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SimulationScenario {
    pub(crate) id: String,
    pub(crate) description: String,
    pub(crate) applicable_rule_ids: Vec<String>,
    pub(crate) allowed_actions: Vec<AllowedAction>,
    #[serde(default)]
    pub(crate) baseline_scenario_id: Option<String>,
    #[serde(default)]
    pub(crate) modeled_action: Option<AllowedAction>,
    #[serde(default)]
    pub(crate) role: Option<SimulationScenarioRole>,
    #[serde(default)]
    pub(crate) command_schedule_binding: Option<String>,
    #[serde(default)]
    pub(crate) state_bindings: Vec<SimulationStateBinding>,
    #[serde(default)]
    pub(crate) constraints: Vec<SimulationConstraint>,
    #[serde(default)]
    pub(crate) thermal: bool,
    pub(crate) duration_days: f64,
    #[serde(default)]
    pub(crate) patches: Vec<SimulationPatchBinding>,
    #[serde(default)]
    pub(crate) parameters: Vec<SimulationParameter>,
    pub(crate) metrics: Vec<SimulationMetric>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SimulationConfig {
    pub(crate) eds_path: PathBuf,
    #[serde(default = "default_max_simulation_runs")]
    pub(crate) max_runs: u8,
    #[serde(default = "default_simulation_timeout_ms")]
    pub(crate) run_timeout_ms: u64,
    pub(crate) scenarios: Vec<SimulationScenario>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LlmConfig {
    pub(crate) adapter: AdapterSelection,
    pub(crate) model: String,
    #[serde(default = "default_request_timeout_ms")]
    pub(crate) request_timeout_ms: u64,
    #[serde(default = "default_response_temperature")]
    pub(crate) response_temperature: f64,
    #[serde(default = "default_max_output_tokens")]
    pub(crate) max_output_tokens: u32,
    #[serde(default = "default_context_window_tokens")]
    pub(crate) context_window_tokens: u32,
    #[serde(default = "default_context_safety_margin_tokens")]
    pub(crate) context_safety_margin_tokens: u32,
}

impl LlmConfig {
    fn validate(&self) -> Result<()> {
        if self.adapter.kind.trim().is_empty() {
            bail!("llm.adapter.kind must not be empty");
        }
        if self.model.trim().is_empty() {
            bail!("llm.model must not be empty");
        }
        if self.request_timeout_ms == 0 {
            bail!("llm.request_timeout_ms must be greater than zero");
        }
        if !self.response_temperature.is_finite() || self.response_temperature < 0.0 {
            bail!("llm.response_temperature must be finite and non-negative");
        }
        if self.max_output_tokens == 0 || self.max_output_tokens > MAX_OUTPUT_TOKENS {
            bail!("llm.max_output_tokens must be between 1 and {MAX_OUTPUT_TOKENS}");
        }
        if self.context_window_tokens == 0 {
            bail!("llm.context_window_tokens must be greater than zero");
        }
        if self.context_safety_margin_tokens >= self.context_window_tokens
            || self
                .max_output_tokens
                .saturating_add(self.context_safety_margin_tokens)
                >= self.context_window_tokens
        {
            bail!(
                "llm.max_output_tokens plus llm.context_safety_margin_tokens must be less than llm.context_window_tokens"
            );
        }
        Ok(())
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            adapter: AdapterSelection {
                kind: "ollama".to_string(),
                config: json!({"endpoint": "http://127.0.0.1:11434/api/generate"}),
            },
            model: default_model(),
            request_timeout_ms: default_request_timeout_ms(),
            response_temperature: default_response_temperature(),
            max_output_tokens: default_max_output_tokens(),
            context_window_tokens: default_context_window_tokens(),
            context_safety_margin_tokens: default_context_safety_margin_tokens(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AnomalyRecoveryModeConfig {
    pub(crate) llm: LlmConfig,
    #[serde(default = "default_max_prompt_chars")]
    pub(crate) max_prompt_chars: usize,
    #[serde(default = "default_max_response_chars")]
    pub(crate) max_response_chars: usize,
    #[serde(default = "default_max_decision_attempts")]
    pub(crate) max_decision_attempts: u8,
    #[serde(default = "default_max_feedback_chars")]
    pub(crate) max_feedback_chars: usize,
    #[serde(default = "default_require_board_snapshot")]
    pub(crate) require_board_snapshot: bool,
    #[serde(default)]
    pub(crate) decision_trace: bool,
    #[serde(default = "default_goal")]
    pub(crate) goal: String,
    #[serde(default = "default_analysis_instructions")]
    pub(crate) analysis_instructions: String,
    #[serde(default)]
    pub(crate) action_catalog: Vec<ActionDefinition>,
    #[serde(default)]
    pub(crate) nominal_profiles: Vec<NominalProfile>,
    #[serde(default)]
    pub(crate) simulation: Option<SimulationConfig>,
}

impl AnomalyRecoveryModeConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.nominal_profiles.is_empty() {
            bail!("anomaly recovery requires at least one nominal profile");
        }
        self.llm.validate()?;
        if self.max_prompt_chars == 0 || self.max_response_chars == 0 {
            bail!("prompt and response character limits must be greater than zero");
        }
        if self.max_decision_attempts == 0 {
            bail!("max_decision_attempts must be greater than zero");
        }
        let mut action_ids = HashSet::new();
        for action in &self.action_catalog {
            if !action.id.is_recommendable() {
                bail!(
                    "action '{}' is not supported for anomaly recommendations",
                    action.id.as_str()
                );
            }
            if action.description.trim().is_empty() {
                bail!(
                    "action '{}': description must not be empty",
                    action.id.as_str()
                );
            }
            if !action_ids.insert(action.id) {
                bail!("action '{}' is defined more than once", action.id.as_str());
            }
        }

        let mut profile_ids = HashSet::new();
        let mut sources = HashSet::new();
        let mut rule_ids = HashSet::new();
        for profile in &self.nominal_profiles {
            if profile.id.trim().is_empty() {
                bail!("nominal profile id must not be empty");
            }
            if profile.source.trim().is_empty() {
                bail!("nominal profile '{}': source must not be empty", profile.id);
            }
            if !profile_ids.insert(profile.id.as_str()) {
                bail!("nominal profile '{}' is defined more than once", profile.id);
            }
            if !sources.insert(profile.source.as_str()) {
                bail!(
                    "source '{}' is assigned to more than one nominal profile",
                    profile.source
                );
            }
            if profile.rules.is_empty() {
                bail!("nominal profile '{}': rules must not be empty", profile.id);
            }

            for rule in &profile.rules {
                rule.validate()?;
                if !rule_ids.insert(rule.id.as_str()) {
                    bail!("nominal rule '{}' is defined more than once", rule.id);
                }
                for action in &rule.eligible_actions {
                    if !action_ids.contains(action) {
                        bail!(
                            "rule '{}': action '{}' is not in action_catalog",
                            rule.id,
                            action.as_str()
                        );
                    }
                }
            }
        }

        if let Some(simulation) = &self.simulation {
            if simulation.eds_path.as_os_str().is_empty()
                || simulation.max_runs < 2
                || simulation.run_timeout_ms == 0
            {
                bail!("simulation requires eds_path, max_runs >= 2, and run_timeout_ms > 0");
            }
            let mut scenario_ids = HashSet::new();
            for scenario in &simulation.scenarios {
                if scenario.id.trim().is_empty()
                    || scenario.description.trim().is_empty()
                    || !scenario.duration_days.is_finite()
                    || scenario.duration_days <= 0.0
                {
                    bail!("simulation scenario has invalid id, description, or duration");
                }
                if !scenario_ids.insert(scenario.id.as_str())
                    || scenario.applicable_rule_ids.is_empty()
                    || scenario.allowed_actions.is_empty()
                    || scenario.metrics.is_empty()
                {
                    bail!(
                        "simulation scenario '{}': duplicate id or missing applicability, actions, or metrics",
                        scenario.id
                    );
                }
                for rule_id in &scenario.applicable_rule_ids {
                    if !rule_ids.contains(rule_id.as_str()) {
                        bail!(
                            "simulation scenario '{}': unknown rule '{}'",
                            scenario.id,
                            rule_id
                        );
                    }
                }
                for action in &scenario.allowed_actions {
                    if !action_ids.contains(action) {
                        bail!(
                            "simulation scenario '{}': action '{}' is not configured",
                            scenario.id,
                            action.as_str()
                        );
                    }
                }
                let mut parameter_ids = HashSet::new();
                for patch in &scenario.patches {
                    if patch.agent_id.trim().is_empty()
                        || patch.engine.trim().is_empty()
                        || patch.field.trim().is_empty()
                        || patch.type_.trim().is_empty()
                        || (patch.value.is_some() == patch.telemetry_path.is_some())
                        || patch.value.is_some_and(|v| !v.is_finite())
                    {
                        bail!(
                            "simulation scenario '{}': invalid trusted patch binding",
                            scenario.id
                        );
                    }
                    if let Some(path) = &patch.telemetry_path {
                        validate_path(path)
                            .map_err(|e| anyhow!("simulation scenario '{}': {e}", scenario.id))?;
                    }
                }
                for parameter in &scenario.parameters {
                    if parameter.id.trim().is_empty()
                        || !parameter_ids.insert(parameter.id.as_str())
                        || parameter.patch_index >= scenario.patches.len()
                        || !parameter.min.is_finite()
                        || !parameter.max.is_finite()
                        || parameter.min > parameter.max
                    {
                        bail!(
                            "simulation scenario '{}': invalid bounded parameter",
                            scenario.id
                        );
                    }
                    if scenario.patches[parameter.patch_index]
                        .telemetry_path
                        .is_some()
                    {
                        bail!(
                            "simulation scenario '{}': parameter cannot replace telemetry patch",
                            scenario.id
                        );
                    }
                }
                let mut metric_ids = HashSet::new();
                for metric in &scenario.metrics {
                    if metric.id.trim().is_empty()
                        || !metric_ids.insert(metric.id.as_str())
                        || metric.quantity.trim().is_empty()
                        || metric.units.trim().is_empty()
                        || metric.target_file.trim().is_empty()
                        || metric.target_file.contains('/')
                        || metric.target_file.contains('\\')
                        || metric.field.trim().is_empty()
                    {
                        bail!("simulation scenario '{}': invalid metric", scenario.id);
                    }
                }
                if scenario.thermal
                    && !scenario
                        .metrics
                        .iter()
                        .any(|metric| metric.quantity == "temperature")
                {
                    bail!(
                        "thermal simulation scenario '{}' needs a temperature metric",
                        scenario.id
                    );
                }
                if scenario.modeled_action.is_some() && scenario.baseline_scenario_id.is_none() {
                    bail!(
                        "recovery scenario '{}' needs baseline_scenario_id",
                        scenario.id
                    );
                }
                if scenario.role == Some(SimulationScenarioRole::Recovery)
                    && (scenario.modeled_action.is_none()
                        || scenario.baseline_scenario_id.is_none()
                        || scenario.command_schedule_binding.is_none())
                {
                    bail!(
                        "recovery scenario '{}' needs modeled_action, baseline_scenario_id, and command_schedule_binding",
                        scenario.id
                    );
                }
                for binding in &scenario.state_bindings {
                    if binding.id.trim().is_empty()
                        || binding.source.trim().is_empty()
                        || validate_path(&binding.path).is_err()
                    {
                        bail!(
                            "simulation scenario '{}': invalid state binding",
                            scenario.id
                        );
                    }
                }
                for constraint in &scenario.constraints {
                    if constraint.metric_id.trim().is_empty()
                        || constraint.units.trim().is_empty()
                        || !constraint.value.is_finite()
                    {
                        bail!("simulation scenario '{}': invalid constraint", scenario.id);
                    }
                }
            }
        }

        Ok(())
    }

    pub(crate) fn profile_for_source(&self, source: &str) -> Option<&NominalProfile> {
        self.nominal_profiles
            .iter()
            .find(|profile| profile.source == source)
    }

    pub(crate) fn action_definition(&self, action: AllowedAction) -> Option<&ActionDefinition> {
        self.action_catalog
            .iter()
            .find(|definition| definition.id == action)
    }
}

impl Default for AnomalyRecoveryModeConfig {
    fn default() -> Self {
        Self {
            llm: LlmConfig::default(),
            max_prompt_chars: default_max_prompt_chars(),
            max_response_chars: default_max_response_chars(),
            max_decision_attempts: default_max_decision_attempts(),
            max_feedback_chars: default_max_feedback_chars(),
            require_board_snapshot: default_require_board_snapshot(),
            decision_trace: false,
            goal: default_goal(),
            analysis_instructions: default_analysis_instructions(),
            action_catalog: Vec::new(),
            nominal_profiles: Vec::new(),
            simulation: None,
        }
    }
}

fn validate_path(path: &str) -> Result<()> {
    if path.trim().is_empty()
        || path
            .split('.')
            .any(|segment| segment.is_empty() || segment.trim() != segment)
    {
        bail!("path must be a non-empty dot-separated payload path");
    }
    Ok(())
}

fn default_min_consecutive_samples() -> usize {
    1
}

fn default_max_simulation_runs() -> u8 {
    2
}
fn default_simulation_timeout_ms() -> u64 {
    10_000
}

fn default_model() -> String {
    "mistral:7b".to_string()
}

fn default_request_timeout_ms() -> u64 {
    20_000
}

fn default_max_prompt_chars() -> usize {
    1_600
}

fn default_max_response_chars() -> usize {
    800
}

fn default_response_temperature() -> f64 {
    0.0
}

fn default_max_output_tokens() -> u32 {
    256
}

fn default_context_window_tokens() -> u32 {
    DEFAULT_CONTEXT_WINDOW_TOKENS
}

fn default_context_safety_margin_tokens() -> u32 {
    DEFAULT_CONTEXT_SAFETY_MARGIN_TOKENS
}

fn default_max_decision_attempts() -> u8 {
    3
}

fn default_max_feedback_chars() -> usize {
    400
}

fn default_require_board_snapshot() -> bool {
    false
}

fn default_goal() -> String {
    "Select a configured immediate action for detected telemetry anomalies.".to_string()
}

fn default_analysis_instructions() -> String {
    "Treat the supplied anomaly candidates as established facts. Select only an action that the candidate explicitly allows."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> AnomalyRecoveryModeConfig {
        serde_json::from_value(serde_json::json!({
            "llm": {
                "adapter": {
                    "kind": "ollama",
                    "config": {"endpoint": "http://127.0.0.1:11434/api/generate"}
                },
                "model": "mistral:7b"
            },
            "action_catalog": [
                {"id": "point_sun_yaw", "description": "Point solar arrays at the sun."}
            ],
            "nominal_profiles": [
                {
                    "id": "example-v1",
                    "source": "example",
                    "rules": [
                        {
                            "id": "temperature_out_of_nominal",
                            "path": "telemetry.temperature_c",
                            "kind": "number_range",
                            "min": -20.0,
                            "max": 45.0,
                            "eligible_actions": ["point_sun_yaw"]
                        }
                    ]
                }
            ]
        }))
        .expect("valid config JSON")
    }

    #[test]
    fn accepts_valid_static_nominal_profile() {
        valid_config().validate().expect("config should validate");
    }

    #[test]
    fn default_tool_call_budget_reserves_context_for_structured_native_calls() {
        let llm = &valid_config().llm;
        assert_eq!(llm.max_output_tokens, 256);
        assert_eq!(llm.context_window_tokens, DEFAULT_CONTEXT_WINDOW_TOKENS);
        assert_eq!(
            llm.context_safety_margin_tokens,
            DEFAULT_CONTEXT_SAFETY_MARGIN_TOKENS
        );
    }

    #[test]
    fn rejects_output_budget_above_provider_ceiling() {
        let mut config = valid_config();
        config.llm.max_output_tokens = MAX_OUTPUT_TOKENS + 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_output_and_margin_that_exhaust_the_context_window() {
        let mut config = valid_config();
        config.llm.context_window_tokens = 512;
        config.llm.max_output_tokens = 256;
        config.llm.context_safety_margin_tokens = 256;
        assert!(config.validate().is_err());
    }

    #[test]
    fn decision_trace_is_disabled_unless_requested() {
        let config = valid_config();
        assert!(!config.decision_trace);

        let config: AnomalyRecoveryModeConfig = serde_json::from_value(serde_json::json!({
            "llm": {
                "adapter": {
                    "kind": "ollama",
                    "config": {"endpoint": "http://127.0.0.1:11434/api/generate"}
                },
                "model": "mistral:7b"
            },
            "decision_trace": true,
            "action_catalog": [
                {"id": "point_sun_yaw", "description": "Point solar arrays at the sun."}
            ],
            "nominal_profiles": [
                {
                    "id": "example-v1",
                    "source": "example",
                    "rules": [
                        {
                            "id": "temperature_out_of_nominal",
                            "path": "telemetry.temperature_c",
                            "kind": "number_range",
                            "max": 45.0,
                            "eligible_actions": ["point_sun_yaw"]
                        }
                    ]
                }
            ]
        }))
        .expect("decision trace config should parse");
        assert!(config.decision_trace);
        config
            .validate()
            .expect("decision trace config should validate");
    }

    #[test]
    fn rejects_rule_actions_missing_from_catalog() {
        let mut config = valid_config();
        config.action_catalog.clear();
        assert!(config.validate().is_err());
    }

    #[test]
    fn accepts_assessment_only_profile_without_actions() {
        let mut config = valid_config();
        config.action_catalog.clear();
        config.nominal_profiles[0].rules[0].eligible_actions.clear();
        config
            .validate()
            .expect("assessment-only configuration should validate");
    }

    #[test]
    fn rejects_duplicate_sources() {
        let mut config = valid_config();
        let mut duplicate = config.nominal_profiles[0].clone();
        duplicate.id = "other".to_string();
        config.nominal_profiles.push(duplicate);
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_noop_as_anomaly_action() {
        let mut config = valid_config();
        config.nominal_profiles[0].rules[0].eligible_actions = vec![AllowedAction::Noop];
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_untrusted_or_invalid_simulation_contract() {
        let mut value = serde_json::to_value(valid_config()).unwrap();
        value["simulation"] = serde_json::json!({
            "eds_path": "/trusted/eds",
            "scenarios": [{
                "id": "thermal", "description": "thermal check",
                "applicable_rule_ids": ["temperature_out_of_nominal"],
                "allowed_actions": ["point_sun_yaw"], "duration_days": 0.1,
                "patches": [{"agent_id":"a", "engine":"power", "field":"temp", "type":"f64", "telemetry_path":"telemetry.temperature_c"}],
                "parameters": [{"id":"bad", "patch_index":0, "min":0.0, "max":1.0}],
                 "metrics": [{"id":"temp", "quantity":"temperature", "units":"C", "target_file":"a.power.jsonl", "field":"temp", "aggregation":"max"}]
            }]
        });
        let config: AnomalyRecoveryModeConfig = serde_json::from_value(value).unwrap();
        assert!(
            config.validate().is_err(),
            "parameter cannot override telemetry binding"
        );
    }

    fn rejects_legacy_ollama_configuration() {
        let mut value: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/static_nominal_profile.json"))
                .expect("fixture should parse");
        value["ollama_host"] = serde_json::json!("127.0.0.1");
        assert!(serde_json::from_value::<AnomalyRecoveryModeConfig>(value).is_err());
    }
}
