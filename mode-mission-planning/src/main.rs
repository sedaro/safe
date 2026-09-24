mod config;
mod planning;
mod runtime;
mod simulation;

use safe::mode_runtime::run_mode;

use crate::config::MissionPlanningConfig;
use crate::runtime::MissionPlanningMode;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_mode::<MissionPlanningConfig, _>(MissionPlanningMode::default()).await
}
