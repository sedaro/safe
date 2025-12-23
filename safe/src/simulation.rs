use std::{env, time::Duration};

use serde::{Deserialize, Serialize};
use tokio::{process::Command as TokioCommand, time::timeout};
use anyhow::Result;

#[derive(Debug, Serialize, Clone)]
pub struct SedaroSimulator {
  path: std::path::PathBuf,
  command: String,
  args: Vec<String>,
  timeout: Duration,
}

impl SedaroSimulator {
  pub fn new(path: std::path::PathBuf) -> Self {
    SedaroSimulator { 
      path,
      command: "./target/release/main".to_string(),
      args: Vec::new(),
      timeout: Duration::MAX,
    }
  }
  pub fn command(mut self, command: String) -> Self {
    self.command = command;
    self
  }
  pub fn args(mut self, args: Vec<String>) -> Self {
    self.args.extend(args.iter().cloned());
    self
  }
  pub fn timeout(mut self, timeout: Duration) -> Self {
    self.timeout = timeout;
    self
  }
  pub async fn run(&self, duration_days: f64, target_path: &std::path::PathBuf) -> Result<std::process::Output> {
    match timeout(
        self.timeout.clone(),
        TokioCommand::new(&self.command)
          .args(vec!["--duration", &(duration_days).to_string(), "--target-config", target_path.to_str().unwrap()]) 
          .args(self.args.clone())
          .current_dir(&self.path)
          // .env("SIMULATION_PATH", self.path.to_str().unwrap())
          // .env("PYTHONPATH", format!("{}/simulation_python:{}/simulation_buildtime_python:{}/simvm_python:{}", self.path.to_str().unwrap(), self.path.to_str().unwrap(), self.path.to_str().unwrap(), env::var("PYTHONPATH").unwrap_or_default()))
          .env("PATH", format!("/Users/sebastianwelsh/Development/sedaro/scf/.venv/bin:{}", env::var("PATH").unwrap_or_default()))
          // .env("SIM_VM_WORKSPACE", self.path.join("generated").to_str().unwrap())
          // .env("SIM_VM_LOCAL", "1")
          .env("LANG", "en_US.UTF-8")
          .env("LC_ALL", "en_US.UTF-8")
          .env("LC_CTYPE", "en_US.UTF-8")
          .output(),
    ).await {
      Err(_) => Err(anyhow::anyhow!("Simulation timed out after {:?} seconds", self.timeout.as_secs())),
      Ok(output) => Ok(output?),
    }
  }
}



#[derive(Debug, Deserialize)]
pub struct FileTargetConfig {
  pub config: String,
  pub stream_id: String,
  #[serde(alias = "type")]
  pub type_: String,
}

#[derive(Debug, Deserialize)]
pub struct FileTargetFrame {
  pub frame: String,
  pub time: f64,
  pub time_step: f64,
}

#[derive(Debug, Deserialize)]
pub struct FileTargetFrameEntry {
  pub data: FileTargetFrame,
  pub event: String,
  pub stream_id: String,
}
#[derive(Debug, Deserialize)]
pub struct FileTargetConfigEntry {
  pub data: FileTargetConfig,
  pub event: String,
  pub stream_id: String,
}

mod tests {
    use base64::{Engine, prelude::BASE64_STANDARD};

    use super::*;

    #[test]
    fn test_deser() {
      let d = "{\"data\": {\"frame\": \"Ojh7A1M4aD4OXAPUmHGxPzo4ewNTOGg+AAAAAAAAAAAAAAAAAAAAAN2GE2yw0FW+c/MadAWsor8AAAAAAAAAgN2GE2yw0FW+AAAAAAAAAIB8vJBmDtUhPn7HaQ7qt82/AAAAAAAAAAAAAAAAAAAAAHy8kGYO1SE+3Lw5s5ZdBL/cvDmzll0kvwAAAAAAAACAabUM7FgVDT7TqOkwv53NvW4PIvlgaBk/bg8i+WBoOT8Axu8aVyQiPgAAAAAAAACAQcL3XHUbEz6Rzddv4er7vpHN12/h6hu/x1lA25hMxD1Ai/nbov70PQAAAAAAAACA8txmVC+buT9BxX77W7cGP97akH4unvW+lgtyagiIUj+ArD0EKvvaPjcLtcZPCfU+pMJSGAH1lr+uLUHo8P3vvwAAAAAAAAAA6hI11QCsxr6vzrMhXAunP3jdFcCy9+8/Aay8s7GXEOi+bZE+n4REtz7VtWt1W9n2Phqyp5WcgbrALqeCeHklg8A5IgZhP9aiv4NPCayU6uU/w9rONlWzHsCUIuwDMq82P43qZTMDTO1AcuUByTpXHj8=\", \"time\": 60000.100024183375, \"time_step\": 0.00011574074074074072}, \"event\": \"enqueue\", \"stream_id\": \"PTnSrPdY4f8c2XSBcZX9LH.gnc\"}";
      let entry: FileTargetFrameEntry = serde_json::from_str(d).unwrap();
      assert_eq!(entry.data.time, 60000.100024183375);
    }
}
