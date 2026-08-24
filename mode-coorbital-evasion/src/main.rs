mod config;
mod planning;
mod runtime;
mod simulation;
mod types;

use safe::mode_runtime::run_mode;

use crate::config::CoorbitalEvasionModeConfig;
use crate::types::CoorbitalEvasionMode;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_mode::<CoorbitalEvasionModeConfig, _>(CoorbitalEvasionMode::new()).await
}
