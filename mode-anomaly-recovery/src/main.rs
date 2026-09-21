mod config;
mod http_client;
mod planner;
mod runtime;
mod types;

use safe::mode_runtime::run_mode;
use safe_llm_adapter::AdapterRegistry;

use crate::config::AnomalyRecoveryModeConfig;
use crate::types::AnomalyRecoveryMode;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_mode::<AnomalyRecoveryModeConfig, _>(AnomalyRecoveryMode::new(
        AdapterRegistry::with_builtin_adapters(),
    ))
    .await
}
