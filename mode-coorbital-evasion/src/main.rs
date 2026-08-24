mod config;
mod planning;
mod runtime;
mod simulation;
mod types;

use safe::mode_runtime::run_mode;

use crate::config::ElectronicWarfareModeConfig;
use crate::types::ElectronicWarfareMode;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_mode::<ElectronicWarfareModeConfig, _>(ElectronicWarfareMode::new()).await
}
