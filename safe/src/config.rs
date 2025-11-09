use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub observability: ObservabilityConfig,
    pub router: RouterConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct C2Config {
    pub transport: String, // "tcp", "udp", "serial", "mock"
    pub address: String,
    pub port: u16,
    pub timeout_ms: u64,
    pub reconnect_interval_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    pub log_level: String, // TODO: Hookup
    pub rotation: String,  // TODO: Hookup
    pub metrics: MetricsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub period_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterConfig {
    pub max_autonomy_modes: usize,
    pub telem_channel_buffer_size: usize,
    pub command_channel_buffer_size: usize,
    pub historic_telem_buffer_size: usize,
    pub period_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // c2: C2Config {
            //     transport: "tcp".to_string(),
            //     address: "127.0.0.1".to_string(),
            //     port: 8001,
            //     timeout_ms: 5000,
            //     reconnect_interval_ms: 1000,
            // },
            observability: ObservabilityConfig {
                log_level: "info".to_string(),
                rotation: "daily".to_string(),
                metrics: MetricsConfig {
                    enabled: true,
                    period_seconds: 10,
                },
            },
            router: RouterConfig {
                max_autonomy_modes: 64,
                telem_channel_buffer_size: 1000,
                command_channel_buffer_size: 100,
                historic_telem_buffer_size: 100,
                period_seconds: 1,
            },
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub enum ConfigMessage {
    SetAction(EngagementMode),
    AddMode { name: String, config: String },
    RemoveMode { name: String },
    QueryTelemetry,
    QueryLogs { start: u64, end: u64 },
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub enum EngagementMode {
    Off,
    Passive,
    Active,
}