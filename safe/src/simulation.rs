use std::env;

use serde::Serialize;
use tokio::process::Command as TokioCommand;
use anyhow::Result;

#[derive(Debug, Serialize, Clone)]
pub struct SedaroSimulator {
  path: std::path::PathBuf,
  command: String,
  args: Vec<String>,
}

impl SedaroSimulator {
  pub fn new(path: std::path::PathBuf, command: &str, args: Option<Vec<&str>>) -> Self {
    SedaroSimulator { 
      path, 
      command: command.to_string(), 
      args: args.unwrap_or_default().into_iter().map(|s| s.to_string()).collect() 
    }
  }
  pub async fn run(&self, duration_s: f64) -> Result<std::process::Output> {
    let output = TokioCommand::new(&self.command)
        // FIXME: confirm duration is decimal days?
        .args(vec!["--duration", &(duration_s/86400.0).to_string(), "--target-config", "TracingTarget"]) 
        .args(&self.args)
        .current_dir(&self.path.join("generated"))
        .env("SIMULATION_PATH", self.path.to_str().unwrap())
        .env("PYTHONPATH", format!("{}/simulation_python:{}/simulation_buildtime_python:{}/simvm_python:{}", self.path.to_str().unwrap(), self.path.to_str().unwrap(), self.path.to_str().unwrap(), env::var("PYTHONPATH").unwrap_or_default()))
        .env("PATH", format!("{}/.venv/bin:{}", self.path.to_str().unwrap(), env::var("PATH").unwrap_or_default()))
        .env("SIM_VM_WORKSPACE", self.path.join("generated").to_str().unwrap())
        .env("SIM_VM_LOCAL", "1")
        .env("LANG", "en_US.UTF-8")
        .env("LC_ALL", "en_US.UTF-8")
        .env("LC_CTYPE", "en_US.UTF-8")
        .output()
        .await?;
    Ok(output)
  }
}
