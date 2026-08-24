mod config;
mod runtime;
mod types;

use safe::mode_runtime::run_mode;

use crate::config::LlmAdvisorModeConfig;
use crate::types::LlmAdvisorMode;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_mode::<LlmAdvisorModeConfig, _>(LlmAdvisorMode::new()).await
}
