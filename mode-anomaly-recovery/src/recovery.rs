//! Deterministic recovery policy. Time and shutdown execution are supplied by the caller.
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::runtime::value_at_payload_path;
use crate::types::TelemetrySample;

pub(crate) const STATE_FILE: &str = "recovery-state.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct MeasurementConfig {
    pub id: String,
    pub source: String,
    pub path: String,
    /// Absolute UTC seconds of this sensor's measurement, not the aggregate frame.
    pub timestamp_path: String,
    pub units: String,
    #[serde(default = "one")]
    pub scale: f64,
    #[serde(default = "one")]
    pub timestamp_scale: f64,
    #[serde(default)]
    pub validity_path: Option<String>,
    #[serde(default)]
    pub valid_value: Option<Value>,
    pub trigger: f64,
    pub recover: f64,
}

fn one() -> f64 {
    1.0
}
fn hold_default() -> u64 {
    6000
}
fn sample_default() -> usize {
    1
}
fn clock_step_default() -> u64 {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecoveryConfig {
    pub power: MeasurementConfig,
    pub thermals: Vec<MeasurementConfig>,
    #[serde(default = "hold_default")]
    pub minimum_hold_secs: u64,
    pub max_measurement_age_secs: u64,
    #[serde(default = "sample_default")]
    pub trigger_samples: usize,
    #[serde(default = "sample_default")]
    pub recovery_samples: usize,
    #[serde(default)]
    pub recovery_dwell_secs: u64,
    /// Deployment assertion: RTC/system UTC is synchronized before SAFE starts.
    pub trusted_system_utc: bool,
    #[serde(default = "clock_step_default")]
    pub max_clock_step_secs: u64,
}

impl RecoveryConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.trusted_system_utc,
            "recovery requires reboot-surviving trusted system UTC"
        );
        ensure!(
            self.minimum_hold_secs > 0 && self.minimum_hold_secs <= 604800,
            "minimum_hold_secs must be in 1..=604800"
        );
        ensure!(
            self.max_measurement_age_secs > 0 && self.max_clock_step_secs > 0,
            "recovery timing limits must be positive"
        );
        ensure!(
            self.trigger_samples > 0 && self.recovery_samples > 0,
            "recovery sample counts must be positive"
        );
        ensure!(
            !self.thermals.is_empty() && self.thermals.len() <= 64,
            "configure 1..=64 thermal channels"
        );
        let mut ids = HashSet::new();
        for (index, measurement) in self.measurements().enumerate() {
            ensure!(
                ids.insert(&measurement.id) && !measurement.id.is_empty(),
                "measurement IDs must be nonempty and unique"
            );
            ensure!(
                !measurement.source.is_empty()
                    && !measurement.path.is_empty()
                    && !measurement.timestamp_path.is_empty()
                    && !measurement.units.is_empty(),
                "measurement source, paths and units are required"
            );
            ensure!(
                measurement.scale.is_finite()
                    && measurement.scale > 0.0
                    && measurement.timestamp_scale.is_finite()
                    && measurement.timestamp_scale > 0.0,
                "measurement scales must be finite and positive"
            );
            ensure!(
                measurement.trigger.is_finite() && measurement.recover.is_finite(),
                "thresholds must be finite"
            );
            ensure!(
                if index == 0 {
                    measurement.recover > measurement.trigger
                } else {
                    measurement.recover < measurement.trigger
                },
                "recovery thresholds must provide hysteresis"
            );
            ensure!(
                measurement.validity_path.is_some() == measurement.valid_value.is_some(),
                "validity_path and valid_value must be configured together"
            );
        }
        Ok(())
    }

    fn measurements(&self) -> impl Iterator<Item = &MeasurementConfig> {
        std::iter::once(&self.power).chain(&self.thermals)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Measurement {
    pub value: Option<f64>,
    pub measured_at: Option<f64>,
    pub valid: bool,
    pub violations: usize,
    pub recovered_samples: usize,
    pub recovered_since: Option<f64>,
}

impl Measurement {
    fn invalidate(&mut self) {
        self.valid = false;
        self.violations = 0;
        self.recovered_samples = 0;
        self.recovered_since = None;
    }

    fn fresh(&self, now: f64, config: &RecoveryConfig) -> bool {
        self.valid
            && self.measured_at.is_some_and(|at| {
                at <= now + config.max_clock_step_secs as f64
                    && now - at <= config.max_measurement_age_secs as f64
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Episode {
    pub schema_version: u32,
    pub id: String,
    pub policy: RecoveryConfig,
    pub reasons: Vec<String>,
    pub trigger_measurements: Vec<Measurement>,
    pub attempted_at: f64,
    pub resume_after: f64,
    pub shutdown_outcome: String,
    pub completed_at: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Decision {
    Hold(String),
    Shutdown(Vec<String>),
    Release,
}

pub(crate) struct RecoveryController {
    pub config: RecoveryConfig,
    pub measurements: Vec<Measurement>,
    pub episode: Option<Episode>,
    pub blocked: Option<String>,
    pub loaded: bool,
    pub clock_valid: bool,
}

impl RecoveryController {
    pub fn new(config: RecoveryConfig) -> Self {
        let measurements = vec![Measurement::default(); config.thermals.len() + 1];
        Self {
            config,
            measurements,
            episode: None,
            blocked: None,
            loaded: false,
            clock_valid: true,
        }
    }

    pub fn load(&mut self, directory: &Path) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let result = (|| -> Result<Option<Episode>> {
            let path = directory.join(STATE_FILE);
            if !path.try_exists()? {
                ensure!(
                    !directory
                        .join(crate::actions::SHUTDOWN_JOURNAL)
                        .try_exists()?,
                    "legacy shutdown journal has no cooldown deadline; explicit migration required"
                );
                return Ok(None);
            }
            ensure!(
                path.metadata()?.len() <= 1024 * 1024,
                "recovery record is oversized"
            );
            let episode: Episode = serde_json::from_reader(File::open(path)?)?;
            ensure!(
                episode.schema_version == 1,
                "unsupported recovery record version"
            );
            episode.policy.validate()?;
            ensure!(
                episode.attempted_at.is_finite()
                    && episode.attempted_at > 0.0
                    && episode.resume_after.is_finite()
                    && episode.resume_after
                        >= episode.attempted_at + episode.policy.minimum_hold_secs as f64,
                "invalid recovery deadline"
            );
            ensure!(
                episode
                    .completed_at
                    .is_none_or(|at| at.is_finite() && at >= episode.resume_after),
                "invalid completion time"
            );
            if episode.completed_at.is_none() {
                ensure!(
                    episode.policy == self.config,
                    "active recovery policy changed; restore its configuration before continuing"
                );
            }
            Ok(Some(episode))
        })();
        match result {
            Ok(episode) => self.episode = episode,
            Err(error) => self.blocked = Some(format!("recovery state unavailable: {error:#}")),
        }
    }

    pub fn observe(&mut self, sample: &TelemetrySample, now: f64) {
        for (index, (config, state)) in self
            .config
            .measurements()
            .zip(&mut self.measurements)
            .enumerate()
        {
            if sample.source.as_deref() != Some(&config.source) {
                continue;
            }
            let value = value_at_payload_path(&sample.payload, &config.path)
                .and_then(Value::as_f64)
                .map(|v| v * config.scale);
            let at = value_at_payload_path(&sample.payload, &config.timestamp_path)
                .and_then(Value::as_f64)
                .map(|v| v * config.timestamp_scale);
            let valid = config.validity_path.as_ref().is_none_or(|path| {
                value_at_payload_path(&sample.payload, path) == config.valid_value.as_ref()
            });
            let (Some(value), Some(at)) = (value, at) else {
                state.invalidate();
                continue;
            };
            if !valid
                || !value.is_finite()
                || !at.is_finite()
                || at <= 0.0
                || at > now + self.config.max_clock_step_secs as f64
                || now - at > self.config.max_measurement_age_secs as f64
            {
                state.invalidate();
                continue;
            }
            if state.measured_at.is_some_and(|previous| at <= previous) {
                continue;
            }
            if state
                .measured_at
                .is_some_and(|previous| at - previous > self.config.max_measurement_age_secs as f64)
            {
                state.invalidate();
            }
            state.measured_at = Some(at);
            state.value = Some(value);
            state.valid = true;
            let critical = if index == 0 {
                value <= config.trigger
            } else {
                value >= config.trigger
            };
            let recovered = if index == 0 {
                value >= config.recover
            } else {
                value <= config.recover
            };
            state.violations = if critical {
                state.violations.saturating_add(1)
            } else {
                0
            };
            if recovered {
                state.recovered_samples = state.recovered_samples.saturating_add(1);
                state.recovered_since.get_or_insert(at);
            } else {
                state.recovered_samples = 0;
                state.recovered_since = None;
            }
        }
    }

    pub fn decision(&self, now: f64) -> Decision {
        if let Some(reason) = &self.blocked {
            return Decision::Hold(reason.clone());
        }
        if !self.loaded {
            return Decision::Hold("loading recovery state".into());
        }
        if !self.clock_valid || !now.is_finite() || now <= 0.0 {
            return Decision::Hold("trusted clock unavailable".into());
        }
        if let Some(episode) = &self.episode {
            if now < episode.attempted_at {
                return Decision::Hold("clock precedes shutdown attempt".into());
            }
            if episode.completed_at.is_none() {
                if now < episode.resume_after {
                    return Decision::Hold("minimum cooldown active".into());
                }
                for (config, state) in self.config.measurements().zip(&self.measurements) {
                    if !state.fresh(now, &self.config) {
                        return Decision::Hold(format!(
                            "{}: fresh valid measurement required",
                            config.id
                        ));
                    }
                    if state.recovered_samples < self.config.recovery_samples
                        || !state.recovered_since.zip(state.measured_at).is_some_and(
                            |(since, latest)| {
                                latest - since >= self.config.recovery_dwell_secs as f64
                            },
                        )
                    {
                        return Decision::Hold(format!(
                            "{}: recovery threshold/persistence not satisfied",
                            config.id
                        ));
                    }
                }
                return Decision::Release;
            }
        }
        let reasons: Vec<_> = self
            .config
            .measurements()
            .zip(&self.measurements)
            .filter(|(_, state)| {
                state.fresh(now, &self.config) && state.violations >= self.config.trigger_samples
            })
            .map(|(config, _)| config.id.clone())
            .collect();
        if !reasons.is_empty() {
            return Decision::Shutdown(reasons);
        }
        for (config, state) in self.config.measurements().zip(&self.measurements) {
            if !state.fresh(now, &self.config) {
                return Decision::Hold(format!("{}: fresh valid measurement required", config.id));
            }
            if state.violations > 0 {
                return Decision::Hold(format!("{}: confirming critical measurement", config.id));
            }
        }
        Decision::Release
    }

    /// At-most-once attempt: uncertain outcomes survive restart as an active hold.
    pub fn reserve_shutdown(
        &mut self,
        directory: &Path,
        now: f64,
        reasons: Vec<String>,
    ) -> Result<()> {
        ensure!(
            matches!(self.decision(now), Decision::Shutdown(_)),
            "shutdown is not eligible"
        );
        let episode = Episode {
            schema_version: 1,
            id: format!("shutdown-{now}"),
            policy: self.config.clone(),
            reasons,
            trigger_measurements: self.measurements.clone(),
            attempted_at: now,
            resume_after: now + self.config.minimum_hold_secs as f64,
            shutdown_outcome: "attempting; host outcome unknown".into(),
            completed_at: None,
        };
        // Set in-memory state before I/O: a partial write must not permit a retry.
        self.episode = Some(episode);
        for measurement in &mut self.measurements {
            measurement.recovered_samples = 0;
            measurement.recovered_since = None;
        }
        self.persist(directory)
    }

    pub fn complete(&mut self, directory: &Path, now: f64) -> Result<()> {
        ensure!(
            self.decision(now) == Decision::Release,
            "recovery cannot release"
        );
        if let Some(episode) = self.episode.as_mut() {
            if episode.completed_at.is_none() {
                episode.completed_at = Some(now);
                self.persist(directory)?;
            }
        }
        Ok(())
    }

    pub fn persist(&mut self, directory: &Path) -> Result<()> {
        let result = durable_write(
            directory,
            STATE_FILE,
            self.episode.as_ref().context("missing recovery episode")?,
        );
        if let Err(error) = &result {
            self.blocked = Some(format!("recovery persistence failed: {error:#}"));
        }
        result
    }
}

pub(crate) fn durable_write(directory: &Path, name: &str, value: &impl Serialize) -> Result<()> {
    let temporary = directory.join(format!("{name}.tmp"));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    std::fs::rename(temporary, directory.join(name))?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const START: f64 = 1_800_000_000.0;

    fn config() -> RecoveryConfig {
        let config: crate::config::AnomalyRecoveryModeConfig = serde_json::from_str(include_str!(
            "../testdata/deterministic_recovery_profile.json"
        ))
        .unwrap();
        config.validate().unwrap();
        config.recovery.unwrap()
    }

    fn sample(at: f64, soc: f64, temperature: f64) -> TelemetrySample {
        TelemetrySample {
            source: Some("example".into()),
            ts_mono: 0,
            payload: json!({"battery":{"soc":soc,"measured_at_unix_secs":at,"valid":true},
            "thermal":{"temperature_c":temperature,"measured_at_unix_secs":at,"valid":true}}),
        }
    }

    fn controller(directory: &Path) -> RecoveryController {
        let mut controller = RecoveryController::new(config());
        controller.load(directory);
        controller
    }

    fn start_episode(controller: &mut RecoveryController, directory: &Path) {
        controller.observe(&sample(START, 0.1, 90.0), START);
        controller.observe(&sample(START + 1.0, 0.1, 90.0), START + 1.0);
        let Decision::Shutdown(reasons) = controller.decision(START + 1.0) else {
            panic!("expected shutdown")
        };
        assert_eq!(reasons, vec!["battery_soc", "compute_temperature"]);
        controller
            .reserve_shutdown(directory, START + 1.0, reasons)
            .unwrap();
    }

    #[test]
    fn startup_requires_valid_fresh_measurements_and_either_condition_can_trip() {
        let directory = tempfile::tempdir().unwrap();
        for (soc, temperature, reason) in [
            (0.15, 50.0, "battery_soc"),
            (0.8, 80.0, "compute_temperature"),
        ] {
            let mut c = controller(directory.path());
            assert!(matches!(c.decision(START), Decision::Hold(_)));
            c.observe(&sample(START, 0.8, 50.0), START);
            assert_eq!(c.decision(START), Decision::Release);
            c.observe(&sample(START + 1.0, soc, temperature), START + 1.0);
            assert!(matches!(c.decision(START + 1.0), Decision::Hold(_)));
            c.observe(&sample(START + 2.0, soc, temperature), START + 2.0);
            assert_eq!(
                c.decision(START + 2.0),
                Decision::Shutdown(vec![reason.into()])
            );
        }
    }

    #[test]
    fn reboot_retains_deadline_and_suppresses_shutdown_even_if_still_critical() {
        let directory = tempfile::tempdir().unwrap();
        let mut c = controller(directory.path());
        start_episode(&mut c, directory.path());
        let deadline = c.episode.as_ref().unwrap().resume_after;
        for elapsed in [1500.0, 3000.0, 6100.0] {
            let mut restarted = controller(directory.path());
            restarted.observe(&sample(START + elapsed, 0.1, 95.0), START + elapsed);
            restarted.observe(
                &sample(START + elapsed + 1.0, 0.1, 95.0),
                START + elapsed + 1.0,
            );
            assert_eq!(restarted.episode.as_ref().unwrap().resume_after, deadline);
            assert!(matches!(
                restarted.decision(START + elapsed + 1.0),
                Decision::Hold(_)
            ));
            assert!(
                restarted
                    .reserve_shutdown(directory.path(), START + elapsed, vec![])
                    .is_err()
            );
        }
    }

    #[test]
    fn existing_uuid_string_episode_restores_without_resetting_cooldown() {
        let directory = tempfile::tempdir().unwrap();
        let mut c = controller(directory.path());
        start_episode(&mut c, directory.path());
        let previous_id = "805cd8c7-008d-5c4c-80cf-7248b159eb1f";
        c.episode.as_mut().unwrap().id = previous_id.into();
        c.persist(directory.path()).unwrap();

        let restarted = controller(directory.path());
        assert!(restarted.blocked.is_none());
        let episode = restarted.episode.as_ref().unwrap();
        assert_eq!(episode.id, previous_id);
        assert_eq!(episode.resume_after, START + 1.0 + 6000.0);
        assert!(matches!(
            restarted.decision(START + 1500.0),
            Decision::Hold(_)
        ));
    }

    #[test]
    fn recovery_requires_minimum_hold_both_thresholds_and_measured_dwell() {
        let directory = tempfile::tempdir().unwrap();
        let mut c = controller(directory.path());
        start_episode(&mut c, directory.path());
        c.observe(&sample(START + 2.0, 0.3, 60.0), START + 2.0);
        c.observe(&sample(START + 12.0, 0.3, 60.0), START + 12.0);
        assert!(matches!(c.decision(START + 12.0), Decision::Hold(_)));
        c.observe(&sample(START + 6002.0, 0.2, 60.0), START + 6002.0);
        assert!(matches!(c.decision(START + 6002.0), Decision::Hold(_)));
        c.observe(&sample(START + 6003.0, 0.3, 60.0), START + 6003.0);
        assert!(
            matches!(c.decision(START + 6013.0), Decision::Hold(_)),
            "wall time alone cannot establish sensor dwell"
        );
        c.observe(&sample(START + 6013.0, 0.3, 60.0), START + 6013.0);
        assert_eq!(c.decision(START + 6013.0), Decision::Release);
        c.complete(directory.path(), START + 6013.0).unwrap();
        let mut restarted = controller(directory.path());
        assert!(restarted.episode.as_ref().unwrap().completed_at.is_some());
        assert!(matches!(
            restarted.decision(START + 6014.0),
            Decision::Hold(_)
        ));
        restarted.observe(&sample(START + 6014.0, 0.1, 50.0), START + 6014.0);
        restarted.observe(&sample(START + 6015.0, 0.1, 50.0), START + 6015.0);
        assert!(
            matches!(restarted.decision(START + 6015.0), Decision::Shutdown(_)),
            "later independent episode must be possible"
        );
    }

    #[test]
    fn duplicates_old_frames_bad_status_and_other_sources_cannot_establish_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let mut c = controller(directory.path());
        start_episode(&mut c, directory.path());
        let at = START + 6002.0;
        c.observe(&sample(at, 0.8, 50.0), at);
        for _ in 0..20 {
            c.observe(&sample(at, 0.8, 50.0), at + 10.0);
        }
        c.observe(&sample(at - 1.0, 0.8, 50.0), at + 10.0);
        assert!(matches!(c.decision(at + 10.0), Decision::Hold(_)));
        let mut wrong_source = sample(at + 10.0, 0.8, 50.0);
        wrong_source.source = Some("unrelated".into());
        c.observe(&wrong_source, at + 10.0);
        assert!(matches!(c.decision(at + 10.0), Decision::Hold(_)));
        let mut invalid = sample(at + 11.0, 0.8, 50.0);
        invalid.payload["battery"]["valid"] = json!(false);
        c.observe(&invalid, at + 11.0);
        assert!(matches!(c.decision(at + 11.0), Decision::Hold(_)));
        assert!(matches!(c.decision(at + 100.0), Decision::Hold(_)));
    }

    #[test]
    fn corruption_legacy_journal_and_changed_policy_hold_without_shutdown() {
        for kind in ["corrupt", "legacy", "policy"] {
            let directory = tempfile::tempdir().unwrap();
            match kind {
                "corrupt" => std::fs::write(directory.path().join(STATE_FILE), b"{").unwrap(),
                "legacy" => std::fs::write(
                    directory.path().join(crate::actions::SHUTDOWN_JOURNAL),
                    b"{}",
                )
                .unwrap(),
                _ => {
                    let mut c = controller(directory.path());
                    start_episode(&mut c, directory.path());
                }
            }
            let mut policy = config();
            if kind == "policy" {
                policy.minimum_hold_secs = 60;
            }
            let mut c = RecoveryController::new(policy);
            c.load(directory.path());
            assert!(c.blocked.is_some());
            c.observe(&sample(START + 1.0, 0.1, 95.0), START + 1.0);
            c.observe(&sample(START + 2.0, 0.1, 95.0), START + 2.0);
            assert!(matches!(c.decision(START + 2.0), Decision::Hold(_)));
        }
    }

    #[test]
    fn clock_uncertainty_cannot_release_or_trigger_and_configuration_rejects_missing_hysteresis() {
        let directory = tempfile::tempdir().unwrap();
        let mut c = controller(directory.path());
        c.observe(&sample(START, 0.8, 50.0), START);
        c.clock_valid = false;
        assert!(matches!(c.decision(START), Decision::Hold(_)));
        let mut policy = config();
        policy.power.recover = policy.power.trigger;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn persistence_failure_never_allows_a_shutdown_retry() {
        let directory = tempfile::tempdir().unwrap();
        let mut c = controller(directory.path());
        c.observe(&sample(START, 0.1, 95.0), START);
        c.observe(&sample(START + 1.0, 0.1, 95.0), START + 1.0);
        assert!(
            c.reserve_shutdown(
                &directory.path().join("missing"),
                START + 1.0,
                vec!["hot".into()]
            )
            .is_err()
        );
        assert!(c.blocked.is_some());
        assert!(matches!(c.decision(START + 1.0), Decision::Hold(_)));
    }
}
