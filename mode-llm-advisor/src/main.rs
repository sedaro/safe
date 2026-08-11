mod config;
mod runtime;
mod types;

use safe::mode_runtime::run_mode;
use tracing::error;

use crate::config::LlmAdvisorModeConfig;
use crate::types::LlmAdvisorMode;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Err(e) = run_mode::<LlmAdvisorModeConfig, _>(LlmAdvisorMode::new()).await {
        error!(reason = %format!("{e:#}"), "llm advisor mode exiting with error");
        return Err(e);
    }
    Ok(())
}
