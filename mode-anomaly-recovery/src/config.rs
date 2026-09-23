use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use safe_llm_adapter::AdapterSelection;
use serde::{Deserialize, Serialize};
use serde_json::json;

const MAX_OUTPUT_TOKENS: u32 = 2_048;
const DEFAULT_CONTEXT_WINDOW_TOKENS: u32 = 2_048;
const DEFAULT_CONTEXT_SAFETY_MARGIN_TOKENS: u32 = 256;
const CONFIG_SCHEMA_VERSION: u32 = 1;
const MAX_CONFIGURED_TEXT_CHARS: usize = 16_384;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AllowedAction {
    PointSunYaw,
    PointNadir,
    ThrusterOff,
    Shutdown,
    CaptureImage,
    Noop,
}

impl AllowedAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::PointSunYaw => "point_sun_yaw",
            Self::PointNadir => "point_nadir",
            Self::ThrusterOff => "thruster_off",
            Self::Shutdown => "shutdown",
            Self::CaptureImage => "capture_image",
            Self::Noop => "noop",
        }
    }

    pub(crate) fn is_recommendable(self) -> bool {
        matches!(
            self,
            Self::PointSunYaw | Self::PointNadir | Self::ThrusterOff | Self::Shutdown
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
    pub(crate) compute_power_binding: Option<String>,
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
    #[serde(default)]
    pub(crate) viability: SimulationViabilityConfig,
    #[serde(default)]
    pub(crate) initialization: Option<crate::eds_inputs::SimulationInitialization>,
    pub(crate) scenarios: Vec<SimulationScenario>,
}

impl SimulationScenario {
    pub(crate) fn has_action_binding(&self) -> bool {
        match self.modeled_action {
            Some(AllowedAction::Shutdown) => {
                self.compute_power_binding.is_some() && self.command_schedule_binding.is_none()
            }
            Some(_) => {
                self.command_schedule_binding.is_some() && self.compute_power_binding.is_none()
            }
            None => false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SimulationViabilityConfig {
    #[serde(default = "default_final_soc_metric")]
    pub(crate) final_soc_metric: String,
    #[serde(default = "default_min_soc_metric")]
    pub(crate) min_soc_metric: String,
    #[serde(default = "default_max_soc_degradation_metric")]
    pub(crate) max_soc_degradation_metric: String,
    #[serde(default = "default_temperature_quantity")]
    pub(crate) temperature_quantity: String,
}

impl Default for SimulationViabilityConfig {
    fn default() -> Self {
        Self {
            final_soc_metric: default_final_soc_metric(),
            min_soc_metric: default_min_soc_metric(),
            max_soc_degradation_metric: default_max_soc_degradation_metric(),
            temperature_quantity: default_temperature_quantity(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LlmConfig {
    pub(crate) adapter: AdapterSelection,
    pub(crate) model: String,
    #[serde(default = "default_enable_tool_calls")]
    pub(crate) enable_tool_calls: bool,
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
            enable_tool_calls: default_enable_tool_calls(),
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
pub(crate) struct PlannerLimitsConfig {
    #[serde(default = "default_max_prompt_chars")]
    pub(crate) max_prompt_chars: usize,
    #[serde(default = "default_max_response_chars")]
    pub(crate) max_response_chars: usize,
    #[serde(default = "default_response_size_multiplier")]
    pub(crate) response_size_multiplier: usize,
    #[serde(default = "default_tool_result_max_chars")]
    pub(crate) tool_result_max_chars: usize,
    #[serde(default = "default_context_tool_output_tokens")]
    pub(crate) context_tool_output_tokens: u32,
    #[serde(default = "default_assessment_rationale_max_chars")]
    pub(crate) assessment_rationale_max_chars: usize,
    #[serde(default = "default_assessment_uncertainty_max_chars")]
    pub(crate) assessment_uncertainty_max_chars: usize,
    #[serde(default = "default_forecast_risk_max_items")]
    pub(crate) forecast_risk_max_items: usize,
    #[serde(default = "default_forecast_risk_max_chars")]
    pub(crate) forecast_risk_max_chars: usize,
    #[serde(default = "default_selection_reason_max_chars")]
    pub(crate) selection_reason_max_chars: usize,
    #[serde(default = "default_textual_assessment_rationale_max_chars")]
    pub(crate) textual_assessment_rationale_max_chars: usize,
    #[serde(default = "default_textual_assessment_uncertainty_max_chars")]
    pub(crate) textual_assessment_uncertainty_max_chars: usize,
    #[serde(default = "default_textual_selection_reason_max_chars")]
    pub(crate) textual_selection_reason_max_chars: usize,
}

impl Default for PlannerLimitsConfig {
    fn default() -> Self {
        Self {
            max_prompt_chars: default_max_prompt_chars(),
            max_response_chars: default_max_response_chars(),
            response_size_multiplier: default_response_size_multiplier(),
            tool_result_max_chars: default_tool_result_max_chars(),
            context_tool_output_tokens: default_context_tool_output_tokens(),
            assessment_rationale_max_chars: default_assessment_rationale_max_chars(),
            assessment_uncertainty_max_chars: default_assessment_uncertainty_max_chars(),
            forecast_risk_max_items: default_forecast_risk_max_items(),
            forecast_risk_max_chars: default_forecast_risk_max_chars(),
            selection_reason_max_chars: default_selection_reason_max_chars(),
            textual_assessment_rationale_max_chars: default_textual_assessment_rationale_max_chars(
            ),
            textual_assessment_uncertainty_max_chars:
                default_textual_assessment_uncertainty_max_chars(),
            textual_selection_reason_max_chars: default_textual_selection_reason_max_chars(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlannerConfig {
    #[serde(default = "default_max_turns")]
    pub(crate) max_turns: u8,
    #[serde(default = "default_total_timeout_ms")]
    pub(crate) total_timeout_ms: u64,
    #[serde(default = "default_provider_attempts")]
    pub(crate) provider_attempts: u8,
    #[serde(default = "default_provider_retry_backoff_ms")]
    pub(crate) provider_retry_backoff_ms: u64,
    #[serde(default = "default_repair_attempts")]
    pub(crate) repair_attempts: u8,
    #[serde(default)]
    pub(crate) limits: PlannerLimitsConfig,
}

impl Default for PlannerConfig {
    fn default() -> Self {
        Self {
            max_turns: default_max_turns(),
            total_timeout_ms: default_total_timeout_ms(),
            provider_attempts: default_provider_attempts(),
            provider_retry_backoff_ms: default_provider_retry_backoff_ms(),
            repair_attempts: default_repair_attempts(),
            limits: PlannerLimitsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromptConfig {
    #[serde(default = "default_planner_instructions")]
    pub(crate) planner_instructions: String,
    #[serde(default = "default_assessment_instructions")]
    pub(crate) assessment_instructions: String,
    #[serde(default = "default_selection_instructions")]
    pub(crate) selection_instructions: String,
    #[serde(default = "default_multiple_calls_repair")]
    pub(crate) multiple_calls_repair: String,
    #[serde(default = "default_selection_repair")]
    pub(crate) selection_repair: String,
    #[serde(default = "default_textual_transport_instructions")]
    pub(crate) textual_transport_instructions: String,
    #[serde(default = "default_textual_assessment_guide")]
    pub(crate) textual_assessment_guide: String,
    #[serde(default = "default_textual_selection_guide")]
    pub(crate) textual_selection_guide: String,
    #[serde(default = "default_textual_generic_guide")]
    pub(crate) textual_generic_guide: String,
    #[serde(default = "default_assessment_tool_description")]
    pub(crate) assessment_tool_description: String,
    #[serde(default = "default_telemetry_tool_description")]
    pub(crate) telemetry_tool_description: String,
    #[serde(default = "default_board_tool_description")]
    pub(crate) board_tool_description: String,
    #[serde(default = "default_selection_tool_description")]
    pub(crate) selection_tool_description: String,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            planner_instructions: default_planner_instructions(),
            assessment_instructions: default_assessment_instructions(),
            selection_instructions: default_selection_instructions(),
            multiple_calls_repair: default_multiple_calls_repair(),
            selection_repair: default_selection_repair(),
            textual_transport_instructions: default_textual_transport_instructions(),
            textual_assessment_guide: default_textual_assessment_guide(),
            textual_selection_guide: default_textual_selection_guide(),
            textual_generic_guide: default_textual_generic_guide(),
            assessment_tool_description: default_assessment_tool_description(),
            telemetry_tool_description: default_telemetry_tool_description(),
            board_tool_description: default_board_tool_description(),
            selection_tool_description: default_selection_tool_description(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidenceConfig {
    #[serde(default = "default_max_evidence_items")]
    pub(crate) max_items: usize,
    #[serde(default = "default_history_samples_per_source")]
    pub(crate) history_samples_per_source: usize,
    #[serde(default = "default_true")]
    pub(crate) require_telemetry: bool,
    #[serde(default = "default_true")]
    pub(crate) require_board: bool,
}

impl Default for EvidenceConfig {
    fn default() -> Self {
        Self {
            max_items: default_max_evidence_items(),
            history_samples_per_source: default_history_samples_per_source(),
            require_telemetry: true,
            require_board: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplanningConfig {
    #[serde(default)]
    pub(crate) require_board_snapshot: bool,
    #[serde(default = "default_replan_on_board_change")]
    pub(crate) replan_on_board_change: bool,
    #[serde(default)]
    pub(crate) failed_plan_retry_ms: u64,
}

impl Default for ReplanningConfig {
    fn default() -> Self {
        Self {
            require_board_snapshot: false,
            replan_on_board_change: default_replan_on_board_change(),
            failed_plan_retry_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObservabilityConfig {
    #[serde(default)]
    pub(crate) decision_trace: bool,
    #[serde(default = "default_trace_max_chars")]
    pub(crate) trace_max_chars: usize,
    #[serde(default = "default_candidate_value_max_chars")]
    pub(crate) candidate_value_max_chars: usize,
    #[serde(default = "default_diagnostic_max_chars")]
    pub(crate) diagnostic_max_chars: usize,
    #[serde(default = "default_simulation_trace_max_files")]
    pub(crate) simulation_trace_max_files: usize,
    #[serde(default = "default_simulation_trace_max_fields")]
    pub(crate) simulation_trace_max_fields: usize,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            decision_trace: false,
            trace_max_chars: default_trace_max_chars(),
            candidate_value_max_chars: default_candidate_value_max_chars(),
            diagnostic_max_chars: default_diagnostic_max_chars(),
            simulation_trace_max_files: default_simulation_trace_max_files(),
            simulation_trace_max_fields: default_simulation_trace_max_fields(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AnomalyRecoveryModeConfig {
    pub(crate) schema_version: u32,
    pub(crate) llm: LlmConfig,
    #[serde(default)]
    pub(crate) planner: PlannerConfig,
    #[serde(default)]
    pub(crate) prompts: PromptConfig,
    #[serde(default)]
    pub(crate) evidence: EvidenceConfig,
    #[serde(default)]
    pub(crate) replanning: ReplanningConfig,
    #[serde(default)]
    pub(crate) observability: ObservabilityConfig,
    #[serde(default)]
    pub(crate) action_catalog: Vec<ActionDefinition>,
    #[serde(default)]
    pub(crate) nominal_profiles: Vec<NominalProfile>,
    #[serde(default)]
    pub(crate) simulation: Option<SimulationConfig>,
}

impl AnomalyRecoveryModeConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            bail!("schema_version must be {CONFIG_SCHEMA_VERSION}");
        }
        if self.nominal_profiles.is_empty() {
            bail!("anomaly recovery requires at least one nominal profile");
        }
        self.llm.validate()?;
        self.validate_runtime_settings()?;
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
            if let Some(initialization) = &simulation.initialization {
                initialization.validate()?;
                if self.profile_for_source(&initialization.source).is_none() {
                    bail!("simulation initialization source must match a nominal profile");
                }
            }
            if simulation.eds_path.as_os_str().is_empty()
                || simulation.max_runs < 2
                || simulation.run_timeout_ms == 0
                || simulation.scenarios.is_empty()
            {
                bail!("simulation requires eds_path, max_runs >= 2, and run_timeout_ms > 0");
            }
            if [
                &simulation.viability.final_soc_metric,
                &simulation.viability.min_soc_metric,
                &simulation.viability.max_soc_degradation_metric,
                &simulation.viability.temperature_quantity,
            ]
            .iter()
            .any(|value| value.trim().is_empty())
            {
                bail!("simulation viability metric and quantity names must not be empty");
            }
            let mut scenario_ids = HashSet::new();
            for scenario in &simulation.scenarios {
                if let Some(initialization) = &simulation.initialization {
                    if scenario.state_bindings.is_empty()
                        || scenario
                            .state_bindings
                            .iter()
                            .any(|b| b.source != initialization.source)
                    {
                        bail!(
                            "initialized scenario needs state bindings for the initialization source"
                        );
                    }
                    match scenario.role {
                        Some(SimulationScenarioRole::Baseline) => {
                            if scenario.modeled_action.is_some()
                                || scenario.command_schedule_binding.is_some()
                                || scenario.compute_power_binding.is_some()
                            {
                                bail!("baseline cannot specify a recovery command");
                            }
                        }
                        Some(SimulationScenarioRole::Recovery) => {
                            if scenario.modeled_action == Some(AllowedAction::Shutdown) {
                                if !scenario.has_action_binding()
                                    || !scenario.allowed_actions.contains(&AllowedAction::Shutdown)
                                    || !initialization.compute_power_bindings.iter().any(|b| {
                                        Some(b.id.as_str())
                                            == scenario.compute_power_binding.as_deref()
                                    })
                                    || scenario.thermal
                                {
                                    bail!(
                                        "shutdown requires an executable power-only compute binding"
                                    );
                                }
                            } else {
                                let binding = initialization.command_schedules.iter().find(|s| {
                                    Some(s.id.as_str())
                                        == scenario.command_schedule_binding.as_deref()
                                });
                                if !scenario.modeled_action.is_some_and(|action| {
                                    scenario.allowed_actions.contains(&action)
                                        && binding
                                            .is_some_and(|b| b.action_modes.contains_key(&action))
                                }) {
                                    bail!(
                                        "recovery scenario has no matching executable command schedule"
                                    );
                                }
                            }
                        }
                        None => bail!("initialized scenarios require an explicit role"),
                    }
                }
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
                if scenario
                    .allowed_actions
                    .iter()
                    .collect::<HashSet<_>>()
                    .len()
                    != scenario.allowed_actions.len()
                {
                    bail!(
                        "simulation scenario '{}': allowed actions must be unique",
                        scenario.id
                    );
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
                        .any(|metric| metric.quantity == simulation.viability.temperature_quantity)
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
                        || !scenario.has_action_binding())
                {
                    bail!(
                        "recovery scenario '{}' needs modeled_action, baseline_scenario_id, and an action-specific binding",
                        scenario.id
                    );
                }
                let mut state_binding_ids = HashSet::new();
                for binding in &scenario.state_bindings {
                    if binding.id.trim().is_empty()
                        || !state_binding_ids.insert(binding.id.as_str())
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
                    if constraint.metric_id != simulation.viability.max_soc_degradation_metric {
                        let metric = scenario
                            .metrics
                            .iter()
                            .find(|metric| metric.id == constraint.metric_id)
                            .ok_or_else(|| anyhow!("simulation scenario '{}': constraint references unknown metric '{}'", scenario.id, constraint.metric_id))?;
                        if metric.units != constraint.units {
                            bail!(
                                "simulation scenario '{}': constraint units do not match metric '{}'",
                                scenario.id,
                                constraint.metric_id
                            );
                        }
                    }
                }
            }
            for scenario in &simulation.scenarios {
                if let Some(baseline_id) = &scenario.baseline_scenario_id {
                    let baseline = simulation
                        .scenarios
                        .iter()
                        .find(|candidate| candidate.id == *baseline_id)
                        .ok_or_else(|| {
                            anyhow!(
                                "simulation scenario '{}': baseline '{}' is not configured",
                                scenario.id,
                                baseline_id
                            )
                        })?;
                    if baseline.role != Some(SimulationScenarioRole::Baseline) {
                        bail!(
                            "simulation scenario '{}': associated baseline must have baseline role",
                            scenario.id
                        );
                    }
                    if simulation.initialization.is_some()
                        && (baseline.state_bindings != scenario.state_bindings
                            || baseline.duration_days != scenario.duration_days
                            || scenario
                                .modeled_action
                                .is_some_and(|a| !baseline.allowed_actions.contains(&a))
                            || scenario
                                .applicable_rule_ids
                                .iter()
                                .any(|id| !baseline.applicable_rule_ids.contains(id)))
                    {
                        bail!(
                            "initialized recovery must share baseline state, horizon and rule applicability"
                        );
                    }
                }
            }
        }

        for rule in self.nominal_profiles.iter().flat_map(|p| &p.rules) {
            if rule.eligible_actions.contains(&AllowedAction::Shutdown) {
                let simulation = self
                    .simulation
                    .as_ref()
                    .ok_or_else(|| anyhow!("shutdown requires simulation"))?;
                if simulation.initialization.is_none()
                    || !simulation.scenarios.iter().any(|s| {
                        s.role == Some(SimulationScenarioRole::Recovery)
                            && s.modeled_action == Some(AllowedAction::Shutdown)
                            && s.applicable_rule_ids.contains(&rule.id)
                    })
                {
                    bail!(
                        "rule '{}': shutdown requires initialized compute-on/shutdown scenarios",
                        rule.id
                    );
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
            schema_version: CONFIG_SCHEMA_VERSION,
            llm: LlmConfig::default(),
            planner: PlannerConfig::default(),
            prompts: PromptConfig::default(),
            evidence: EvidenceConfig::default(),
            replanning: ReplanningConfig::default(),
            observability: ObservabilityConfig::default(),
            action_catalog: Vec::new(),
            nominal_profiles: Vec::new(),
            simulation: None,
        }
    }
}

impl AnomalyRecoveryModeConfig {
    fn validate_runtime_settings(&self) -> Result<()> {
        let limits = &self.planner.limits;
        if self.planner.max_turns == 0
            || self.planner.total_timeout_ms == 0
            || self.planner.provider_attempts == 0
            || self.planner.repair_attempts == 0
        {
            bail!(
                "planner turns, timeout, provider attempts, and repair attempts must be greater than zero"
            );
        }
        let positive_limits = [
            limits.max_prompt_chars,
            limits.max_response_chars,
            limits.response_size_multiplier,
            limits.tool_result_max_chars,
            limits.context_tool_output_tokens as usize,
            limits.assessment_rationale_max_chars,
            limits.assessment_uncertainty_max_chars,
            limits.forecast_risk_max_items,
            limits.forecast_risk_max_chars,
            limits.selection_reason_max_chars,
            limits.textual_assessment_rationale_max_chars,
            limits.textual_assessment_uncertainty_max_chars,
            limits.textual_selection_reason_max_chars,
            self.evidence.max_items,
            self.evidence.history_samples_per_source,
            self.observability.trace_max_chars,
            self.observability.candidate_value_max_chars,
            self.observability.diagnostic_max_chars,
            self.observability.simulation_trace_max_files,
            self.observability.simulation_trace_max_fields,
        ];
        if positive_limits.contains(&0) {
            bail!("planner, evidence, and observability limits must be greater than zero");
        }
        if self.planner.max_turns > 32
            || self.planner.provider_attempts > 10
            || self.planner.repair_attempts > 32
            || self.planner.total_timeout_ms > 3_600_000
            || self.planner.provider_retry_backoff_ms > 60_000
            || positive_limits.iter().any(|value| *value > 1_000_000)
        {
            bail!("planner, evidence, or observability setting exceeds its safe upper bound");
        }
        if limits.context_tool_output_tokens > self.llm.max_output_tokens {
            bail!(
                "planner.limits.context_tool_output_tokens must not exceed llm.max_output_tokens"
            );
        }
        if limits.textual_assessment_rationale_max_chars > limits.assessment_rationale_max_chars
            || limits.textual_assessment_uncertainty_max_chars
                > limits.assessment_uncertainty_max_chars
            || limits.textual_selection_reason_max_chars > limits.selection_reason_max_chars
        {
            bail!("textual response limits must not exceed their planner response limits");
        }
        for (name, text) in [
            ("planner_instructions", &self.prompts.planner_instructions),
            (
                "assessment_instructions",
                &self.prompts.assessment_instructions,
            ),
            (
                "selection_instructions",
                &self.prompts.selection_instructions,
            ),
            ("multiple_calls_repair", &self.prompts.multiple_calls_repair),
            ("selection_repair", &self.prompts.selection_repair),
            (
                "textual_transport_instructions",
                &self.prompts.textual_transport_instructions,
            ),
            (
                "textual_assessment_guide",
                &self.prompts.textual_assessment_guide,
            ),
            (
                "textual_selection_guide",
                &self.prompts.textual_selection_guide,
            ),
            ("textual_generic_guide", &self.prompts.textual_generic_guide),
            (
                "assessment_tool_description",
                &self.prompts.assessment_tool_description,
            ),
            (
                "telemetry_tool_description",
                &self.prompts.telemetry_tool_description,
            ),
            (
                "board_tool_description",
                &self.prompts.board_tool_description,
            ),
            (
                "selection_tool_description",
                &self.prompts.selection_tool_description,
            ),
        ] {
            let chars = text.chars().count();
            if text.trim().is_empty() || chars > MAX_CONFIGURED_TEXT_CHARS {
                bail!(
                    "prompts.{name} must contain 1 through {MAX_CONFIGURED_TEXT_CHARS} characters"
                );
            }
        }
        Ok(())
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
fn default_final_soc_metric() -> String {
    "final_state_of_charge".to_string()
}
fn default_min_soc_metric() -> String {
    "minimum_state_of_charge".to_string()
}
fn default_max_soc_degradation_metric() -> String {
    "maximum_state_of_charge_degradation".to_string()
}
fn default_temperature_quantity() -> String {
    "temperature".to_string()
}

fn default_model() -> String {
    "mistral:7b".to_string()
}

fn default_request_timeout_ms() -> u64 {
    20_000
}

fn default_enable_tool_calls() -> bool {
    true
}

fn default_max_prompt_chars() -> usize {
    1_600
}

fn default_response_size_multiplier() -> usize {
    8
}
fn default_tool_result_max_chars() -> usize {
    2_000
}
fn default_context_tool_output_tokens() -> u32 {
    128
}
fn default_assessment_rationale_max_chars() -> usize {
    400
}
fn default_assessment_uncertainty_max_chars() -> usize {
    160
}
fn default_forecast_risk_max_items() -> usize {
    2
}
fn default_forecast_risk_max_chars() -> usize {
    100
}
fn default_selection_reason_max_chars() -> usize {
    200
}
fn default_textual_assessment_rationale_max_chars() -> usize {
    200
}
fn default_textual_assessment_uncertainty_max_chars() -> usize {
    100
}
fn default_textual_selection_reason_max_chars() -> usize {
    120
}
fn default_max_turns() -> u8 {
    6
}
fn default_total_timeout_ms() -> u64 {
    120_000
}
fn default_provider_attempts() -> u8 {
    1
}
fn default_provider_retry_backoff_ms() -> u64 {
    250
}
fn default_repair_attempts() -> u8 {
    3
}
fn default_max_evidence_items() -> usize {
    16
}
fn default_history_samples_per_source() -> usize {
    8
}
fn default_true() -> bool {
    true
}
fn default_replan_on_board_change() -> bool {
    true
}
fn default_trace_max_chars() -> usize {
    1_000
}
fn default_candidate_value_max_chars() -> usize {
    120
}
fn default_diagnostic_max_chars() -> usize {
    240
}
fn default_simulation_trace_max_files() -> usize {
    16
}
fn default_simulation_trace_max_fields() -> usize {
    32
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

fn default_planner_instructions() -> String {
    "Build an evidence-backed assessment or select an eligible action when available.".to_string()
}
fn default_assessment_instructions() -> String {
    "Assess the configured anomaly candidates using the supplied evidence.".to_string()
}
fn default_selection_instructions() -> String {
    "Select only an action explicitly allowed by a supplied candidate.".to_string()
}
fn default_multiple_calls_repair() -> String {
    "Use the only available tool now to complete the requested task with the supplied values."
        .to_string()
}
fn default_selection_repair() -> String {
    "Retry select_recovery_action with a non-empty reason and exact configured IDs.".to_string()
}
fn default_textual_transport_instructions() -> String {
    "Reply with exactly one compact JSON object containing only the operation arguments. Do not repeat or summarize the input. Do not include markdown or explanatory text.".to_string()
}
fn default_textual_assessment_guide() -> String {
    "Emit required keys in this exact order: outcome, disposition, candidate_ids, evidence_ids, rationale, uncertainty. Use exact enum and ID values from the schema. Omit optional fields.".to_string()
}
fn default_textual_selection_guide() -> String {
    "Emit required keys in this exact order: assessment_id, anomaly_id, action_id, reason. Use exact ID values from the schema.".to_string()
}
fn default_textual_generic_guide() -> String {
    "Emit every required field before any optional field.".to_string()
}
fn default_assessment_tool_description() -> String {
    "Complete the evidence-backed thermal assessment; this may finish without a recovery command"
        .to_string()
}
fn default_telemetry_tool_description() -> String {
    "Get the latest telemetry snapshot received from SAFE".to_string()
}
fn default_board_tool_description() -> String {
    "Get the current command board snapshot received from SAFE".to_string()
}
fn default_selection_tool_description() -> String {
    "Choose one eligible recovery action only after a thermal anomaly assessment requests recovery evaluation".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> AnomalyRecoveryModeConfig {
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
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
        assert!(llm.enable_tool_calls);
        assert_eq!(llm.max_output_tokens, 256);
        assert_eq!(llm.context_window_tokens, DEFAULT_CONTEXT_WINDOW_TOKENS);
        assert_eq!(
            llm.context_safety_margin_tokens,
            DEFAULT_CONTEXT_SAFETY_MARGIN_TOKENS
        );
    }

    #[test]
    fn tool_calls_can_be_disabled_for_textual_json_completions() {
        let mut value = serde_json::to_value(valid_config()).unwrap();
        value["llm"]["enable_tool_calls"] = serde_json::json!(false);
        let config: AnomalyRecoveryModeConfig = serde_json::from_value(value).unwrap();
        assert!(!config.llm.enable_tool_calls);
        config.validate().expect("textual mode should validate");
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
        assert!(!config.observability.decision_trace);

        let config: AnomalyRecoveryModeConfig = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "llm": {
                "adapter": {
                    "kind": "ollama",
                    "config": {"endpoint": "http://127.0.0.1:11434/api/generate"}
                },
                "model": "mistral:7b"
            },
            "observability": {"decision_trace": true},
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
        assert!(config.observability.decision_trace);
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

    #[test]
    fn rejects_legacy_ollama_configuration() {
        let mut value: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/static_nominal_profile.json"))
                .expect("fixture should parse");
        value["ollama_host"] = serde_json::json!("127.0.0.1");
        assert!(serde_json::from_value::<AnomalyRecoveryModeConfig>(value).is_err());
    }

    #[test]
    fn checked_in_autonomy_config_contains_a_valid_anomaly_mode_config() {
        let entries: serde_json::Value =
            serde_json::from_str(include_str!("../../safe/autonomy_mode_config.json"))
                .expect("autonomy config should be JSON");
        let value = entries[0]["mode_config"].clone();
        let config: AnomalyRecoveryModeConfig =
            serde_json::from_value(value).expect("anomaly mode config should match the schema");
        config
            .validate()
            .expect("anomaly mode config should validate");
    }

    #[test]
    fn rejects_removed_flat_planner_fields() {
        let mut value = serde_json::to_value(valid_config()).unwrap();
        value["goal"] = serde_json::json!("legacy");
        assert!(serde_json::from_value::<AnomalyRecoveryModeConfig>(value).is_err());
    }
}
