use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Yaml},
};
use serde::Deserialize;

use crate::config_paths::resolve_runtime_config_path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub tracing: TracingConfig,
    pub sockets: SocketsConfig,
    pub logging: LoggingConfig,
    pub limits: LimitsConfig,
    pub platform: PlatformConfig,
    pub base_paths: BasePathsConfig,
    #[serde(default = "default_gatekeeper_config")]
    pub gatekeeper: serde_json::Value,
}

fn default_gatekeeper_config() -> serde_json::Value {
    serde_json::json!({})
}

impl Config {
    pub fn load() -> Result<Self, figment::Error> {
        let safe_config_path = resolve_runtime_config_path();
        let safe_config_dir = safe_config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();

        let figment = Figment::from(Serialized::defaults(SerializedDefaults::default()))
            .merge(Yaml::file(&safe_config_path))
            .merge(Env::prefixed("SAFE_").split("__"));

        figment.extract()
    }

    pub fn telemetry_addr(&self) -> String {
        let addr = SocketAddr::new(self.sockets.telemetry.ip, self.sockets.telemetry.port);
        addr.to_string()
    }

    pub fn commands_addr(&self) -> String {
        let addr = SocketAddr::new(self.sockets.commands.ip, self.sockets.commands.port);
        addr.to_string()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.limits.max_autonomy_modes == 0 {
            return Err("limits.max_items must be > 0".into());
        }
        if self.logging.rotation.max_files == 0 {
            return Err("logging.rotation.max_files must be > 0".into());
        }
        if self.logging.rotation.max_file_size_mb == 0 {
            return Err("logging.rotation.max_file_size_mb must be > 0".into());
        }
        if self
            .logging
            .rotation
            .max_file_size_mb
            .checked_mul(1024 * 1024)
            .is_none()
        {
            return Err("logging.rotation.max_file_size_mb is too large".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TracingConfig {
    pub level: String,
    pub filter: String,
    pub with_target: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SocketsConfig {
    pub telemetry: SocketConfig,
    pub commands: SocketConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SocketConfig {
    pub ip: IpAddr,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    pub file_path: String,
    pub rotation: RotationConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RotationConfig {
    pub max_file_size_mb: u64,
    pub max_files: usize,
    pub daily: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LimitsConfig {
    pub max_autonomy_modes: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlatformConfig {
    #[serde(default = "default_telemetry_adapter")]
    pub telemetry_adapter: String,
    #[serde(default = "default_command_adapter")]
    pub command_adapter: String,
    #[serde(default = "default_egress_adapter")]
    pub egress_adapter: String,
    #[serde(default = "default_gatekeeper_adapter")]
    pub gatekeeper_adapter: String,
    #[serde(default)]
    pub bash_mock_telemetry_command: Option<String>,
    #[serde(default)]
    pub external_telemetry_command: Option<String>,
    #[serde(default)]
    pub external_egress_command: Option<String>,
    #[serde(default)]
    pub external_gatekeeper_command: Option<String>,
}

fn default_telemetry_adapter() -> String {
    "example".to_string()
}

fn default_command_adapter() -> String {
    "safectl_unix_json".to_string()
}

fn default_egress_adapter() -> String {
    "safectl_filesystem".to_string()
}

fn default_gatekeeper_adapter() -> String {
    "disabled".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct BasePathsConfig {
    pub base_working_directory: String,
    pub base_writable_directory: String,
}

#[derive(serde::Serialize)]
struct SerializedDefaults {
    tracing: TracingConfigDefaults,
    sockets: SocketsConfigDefaults,
    logging: LoggingConfigDefaults,
    limits: LimitsConfigDefaults,
    platform: PlatformConfigDefaults,
    base_paths: BasePathsConfigDefaults,
    gatekeeper: serde_json::Value,
}

impl Default for SerializedDefaults {
    fn default() -> Self {
        Self {
            tracing: TracingConfigDefaults {
                level: "info".into(),
                filter: "info".into(),
                with_target: true,
            },
            sockets: SocketsConfigDefaults {
                telemetry: SocketConfigDefaults {
                    ip: "0.0.0.0".into(),
                    port: 44212,
                },
                commands: SocketConfigDefaults {
                    ip: "127.0.0.1".into(),
                    port: 7002,
                },
            },
            logging: LoggingConfigDefaults {
                file_path: "/tmp/safe/logs/app.log".into(),
                rotation: RotationConfigDefaults {
                    max_file_size_mb: 100,
                    max_files: 10,
                    daily: false,
                },
            },
            limits: LimitsConfigDefaults {
                max_autonomy_modes: 10,
            },
            platform: PlatformConfigDefaults {
                telemetry_adapter: default_telemetry_adapter(),
                command_adapter: default_command_adapter(),
                egress_adapter: default_egress_adapter(),
                gatekeeper_adapter: default_gatekeeper_adapter(),
                bash_mock_telemetry_command: None,
                external_telemetry_command: None,
                external_egress_command: None,
                external_gatekeeper_command: None,
            },
            base_paths: BasePathsConfigDefaults {
                base_working_directory: "/tmp/safe".into(),
                base_writable_directory: "/tmp/safe".into(),
            },
            gatekeeper: default_gatekeeper_config(),
        }
    }
}

#[derive(serde::Serialize)]
struct TracingConfigDefaults {
    level: String,
    filter: String,
    with_target: bool,
}

#[derive(serde::Serialize)]
struct SocketsConfigDefaults {
    telemetry: SocketConfigDefaults,
    commands: SocketConfigDefaults,
}

#[derive(serde::Serialize)]
struct SocketConfigDefaults {
    ip: String,
    port: u16,
}

#[derive(serde::Serialize)]
struct LoggingConfigDefaults {
    file_path: String,
    rotation: RotationConfigDefaults,
}

#[derive(serde::Serialize)]
struct BasePathsConfigDefaults {
    base_working_directory: String,
    base_writable_directory: String,
}

#[derive(serde::Serialize)]
struct RotationConfigDefaults {
    max_file_size_mb: u64,
    max_files: usize,
    daily: bool,
}

#[derive(serde::Serialize)]
struct LimitsConfigDefaults {
    max_autonomy_modes: usize,
}

#[derive(serde::Serialize)]
struct PlatformConfigDefaults {
    telemetry_adapter: String,
    command_adapter: String,
    egress_adapter: String,
    gatekeeper_adapter: String,
    bash_mock_telemetry_command: Option<String>,
    external_telemetry_command: Option<String>,
    external_egress_command: Option<String>,
    external_gatekeeper_command: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_prefers_safe_runtime_config_over_path() {
        let td = tempfile::tempdir().unwrap();
        let cfg_a = td.path().join("a.yaml");
        let cfg_b = td.path().join("b.yaml");

        std::fs::write(
            &cfg_a,
            "base_paths:\n  base_writable_directory: /tmp/a\n  base_working_directory: /tmp/a\nplatform:\n  telemetry_adapter: example\n  command_adapter: safectl_unix_json\n  bash_mock_telemetry_command: null\n  gatekeeper_adapter: disabled\n",
        )
        .unwrap();
        std::fs::write(
            &cfg_b,
            "base_paths:\n  base_writable_directory: /tmp/b\n  base_working_directory: /tmp/b\nplatform:\n  telemetry_adapter: example\n  command_adapter: safectl_unix_json\n  bash_mock_telemetry_command: null\n  gatekeeper_adapter: disabled\n",
        )
        .unwrap();

        unsafe {
            std::env::set_var("SAFE_RUNTIME_CONFIG", cfg_a.to_string_lossy().to_string());
            std::env::set_var(
                "SAFE_RUNTIME_CONFIG_PATH",
                cfg_b.to_string_lossy().to_string(),
            );
        }

        let cfg = Config::load().unwrap();
        assert_eq!(cfg.base_paths.base_writable_directory, "/tmp/a");
    }

    #[test]
    fn rejects_log_size_that_overflows_bytes() {
        let mut config: Config = Figment::from(Serialized::defaults(SerializedDefaults::default()))
            .extract()
            .unwrap();
        config.logging.rotation.max_file_size_mb = u64::MAX;

        assert!(config.validate().is_err());
    }
}
