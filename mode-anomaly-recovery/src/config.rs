use std::collections::HashSet;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AnomalyRecoveryModeConfig {
    #[serde(default = "default_ollama_host")]
    pub(crate) ollama_host: String,
    #[serde(default = "default_ollama_port")]
    pub(crate) ollama_port: u16,
    #[serde(default = "default_ollama_path")]
    pub(crate) ollama_path: String,
    #[serde(default = "default_model")]
    pub(crate) model: String,
    #[serde(default = "default_request_timeout_ms")]
    pub(crate) request_timeout_ms: u64,
    #[serde(default = "default_max_prompt_chars")]
    pub(crate) max_prompt_chars: usize,
    #[serde(default = "default_max_response_chars")]
    pub(crate) max_response_chars: usize,
    #[serde(default = "default_response_temperature")]
    pub(crate) response_temperature: f64,
    #[serde(default = "default_num_predict")]
    pub(crate) num_predict: u32,
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
}

impl AnomalyRecoveryModeConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.nominal_profiles.is_empty() {
            bail!("anomaly recovery requires at least one nominal profile");
        }
        if self.action_catalog.is_empty() {
            bail!("anomaly recovery requires an action_catalog");
        }
        if self.request_timeout_ms == 0 {
            bail!("request_timeout_ms must be greater than zero");
        }
        if self.max_prompt_chars == 0 || self.max_response_chars == 0 {
            bail!("prompt and response character limits must be greater than zero");
        }
        if !self.response_temperature.is_finite() || self.response_temperature < 0.0 {
            bail!("response_temperature must be finite and non-negative");
        }
        if self.num_predict == 0 || self.max_decision_attempts == 0 {
            bail!("num_predict and max_decision_attempts must be greater than zero");
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
            ollama_host: default_ollama_host(),
            ollama_port: default_ollama_port(),
            ollama_path: default_ollama_path(),
            model: default_model(),
            request_timeout_ms: default_request_timeout_ms(),
            max_prompt_chars: default_max_prompt_chars(),
            max_response_chars: default_max_response_chars(),
            response_temperature: default_response_temperature(),
            num_predict: default_num_predict(),
            max_decision_attempts: default_max_decision_attempts(),
            max_feedback_chars: default_max_feedback_chars(),
            require_board_snapshot: default_require_board_snapshot(),
            decision_trace: false,
            goal: default_goal(),
            analysis_instructions: default_analysis_instructions(),
            action_catalog: Vec::new(),
            nominal_profiles: Vec::new(),
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

fn default_ollama_host() -> String {
    "127.0.0.1".to_string()
}

fn default_ollama_port() -> u16 {
    11434
}

fn default_ollama_path() -> String {
    "/api/generate".to_string()
}

fn default_model() -> String {
    "mistral:7b".to_string()
}

fn default_request_timeout_ms() -> u64 {
    20_000
}

fn default_max_prompt_chars() -> usize {
    3_500
}

fn default_max_response_chars() -> usize {
    800
}

fn default_response_temperature() -> f64 {
    0.0
}

fn default_num_predict() -> u32 {
    256
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
    fn decision_trace_is_disabled_unless_requested() {
        let config = valid_config();
        assert!(!config.decision_trace);

        let config: AnomalyRecoveryModeConfig = serde_json::from_value(serde_json::json!({
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
    fn rejects_profiles_without_action_catalog_entries() {
        let mut config = valid_config();
        config.action_catalog.clear();
        assert!(config.validate().is_err());
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
}
