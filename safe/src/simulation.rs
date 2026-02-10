use std::io::SeekFrom;
use std::{env, time::Duration};

use base64::prelude::BASE64_STANDARD;
use bytes::Buf;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncSeekExt;
use tokio::{io::BufReader, io::AsyncBufReadExt, process::Command as TokioCommand, time::timeout};
use anyhow::Result;
use tracing_subscriber::fmt::format;
use simvm::sv::data::{Data, FloatValue};
use simvm::sv::ser_de::{dyn_de, dyn_ser};
use simvm::sv::{combine::TR, data::Datum, parse::Parse};
use base64::Engine;
use tokio::fs::File;

use crate::simulation;

#[derive(Debug, Serialize, Clone)]
pub struct SedaroSimulator {
  path: std::path::PathBuf,
  args: Vec<String>,
  timeout: Duration,
  release: bool,
  venv: Option<std::path::PathBuf>,
}

impl SedaroSimulator {
  pub fn new(path: std::path::PathBuf) -> Self {
    SedaroSimulator { 
      path,
      args: Vec::new(),
      timeout: Duration::MAX,
      release: true,
      venv: None,
    }
  }
  pub fn args(mut self, args: Vec<&str>) -> Self {
    self.args.extend(args.iter().map(|s| s.to_string()));
    self
  }
  pub fn timeout(mut self, timeout: Duration) -> Self {
    self.timeout = timeout;
    self
  }
  pub fn debug(mut self) -> Self {
    self.release = false;
    self
  }
  pub fn venv(mut self, venv_path: std::path::PathBuf) -> Self {
    self.venv = Some(venv_path);
    self
  }
  pub async fn run(&self, duration_days: f64, target_path: &std::path::PathBuf) -> Result<std::process::Output> {
    let command = if self.release { "./target/release/main" } else { "./target/debug/main" };
    match timeout(
        self.timeout,
        TokioCommand::new(&command)
          .args(vec!["--duration", &(duration_days).to_string(), "--target-config", target_path.to_str().unwrap()]) 
          .args(self.args.clone())
          .current_dir(&self.path)
          // .env("SIMULATION_PATH", self.path.to_str().unwrap())
          // .env("PYTHONPATH", format!("{}/simulation_python:{}/simulation_buildtime_python:{}/simvm_python:{}", self.path.to_str().unwrap(), self.path.to_str().unwrap(), self.path.to_str().unwrap(), env::var("PYTHONPATH").unwrap_or_default()))
          .env("PATH", match self.venv {
            Some(ref venv_path) => format!("{}/bin:{}", venv_path.to_str().unwrap(), env::var("PATH").unwrap_or_default()),
            None => env::var("PATH").unwrap_or_default(),
          })
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

#[derive(Debug)]
pub struct FileTargetReader {
  reader: BufReader<File>,
  ty: Option<TR>,
  timestamps_mjd: Vec<f64>,
  frames: Vec<Datum>,
  line_idx: u64,
}

impl FileTargetReader {
  
  pub async fn try_from_path(target_file_path: &std::path::PathBuf) -> Result<Self> {
    let file = File::open(target_file_path).await?;
    Ok(FileTargetReader {
      reader: BufReader::new(file),
      ty: None,
      timestamps_mjd: vec![],
      frames: vec![],
      line_idx: 0,
    })
  }

  async fn parse_frames(&mut self) -> Result<()> {
    self.reader.seek(SeekFrom::Start(self.line_idx)).await?;
    loop {
      let mut line = String::new();
      let bytes_read = self.reader.read_line(&mut line).await?;
      if bytes_read == 0 {
        break; // Reached end of file
      }
      if self.ty.is_none() {
        if let Ok(config) = serde_json::from_str::<simulation::FileTargetConfigEntry>(&line) {
          self.ty = Some(TR::parse(&config.data.type_).unwrap());
        }
      }
      else if let Ok(entry) = serde_json::from_str::<simulation::FileTargetFrameEntry>(&line) {
        if let Some(parsed) = &self.ty {
          let frame_bytes = BASE64_STANDARD.decode(&entry.data.frame).unwrap();
          match dyn_de(&parsed.typ, &frame_bytes) {
            Ok(val) => {
              self.timestamps_mjd.push(entry.data.time);
              self.frames.push(val);
            },
            Err(e) => {
              return Err(anyhow::anyhow!("Simulation frame deserialization error: {:?}", e));
            },
          }
        }
      }
      self.line_idx += 1;
    }
    Ok(())
  }

  pub async fn read_frames(&mut self) -> Result<Vec<Datum>> {
    self.parse_frames().await?;
    let frames = self.frames.clone();
    Ok(frames)
  }

  fn idx_of_timestamp(&self, timestamp_mjd: f64) -> usize {
    match self.timestamps_mjd.binary_search_by(|&probe| probe.partial_cmp(&timestamp_mjd).unwrap()) { // TODO: Write test for edge cases of search
      Ok(idx) => idx,
      Err(idx) => idx,
    }
  }
  pub async fn read_frame_at_timestamp(&mut self, timestamp_mjd: f64) -> Result<Option<Datum>> {
    self.parse_frames().await?;
    if let Some(start_time_mjd) = self.timestamps_mjd.first() {
      if timestamp_mjd < *start_time_mjd || timestamp_mjd > *self.timestamps_mjd.last().unwrap() {
        return Ok(None);
      }
    } else {
      return Ok(None);
    }

    Ok(Some(self.frames[self.idx_of_timestamp(timestamp_mjd)].clone()))
  }
  pub async fn read_frame_at_elapsed(&mut self, duration: Duration) -> Result<Option<Datum>> {
    if let Some(start_time_mjd) = self.timestamps_mjd.first() {
      let timestamp_mjd = start_time_mjd + (duration.as_secs_f64() / 86400.0);
      self.read_frame_at_timestamp(timestamp_mjd).await
    } else {
      Ok(None)
    }
  }
}

// TODO: Implement transpose?

#[cfg(test)]
mod tests {
    use crate::simulation::FileTargetFrameEntry;

    #[test]
    fn test_deser() {
      let d = "{\"data\": {\"frame\": \"Ojh7A1M4aD4OXAPUmHGxPzo4ewNTOGg+AAAAAAAAAAAAAAAAAAAAAN2GE2yw0FW+c/MadAWsor8AAAAAAAAAgN2GE2yw0FW+AAAAAAAAAIB8vJBmDtUhPn7HaQ7qt82/AAAAAAAAAAAAAAAAAAAAAHy8kGYO1SE+3Lw5s5ZdBL/cvDmzll0kvwAAAAAAAACAabUM7FgVDT7TqOkwv53NvW4PIvlgaBk/bg8i+WBoOT8Axu8aVyQiPgAAAAAAAACAQcL3XHUbEz6Rzddv4er7vpHN12/h6hu/x1lA25hMxD1Ai/nbov70PQAAAAAAAACA8txmVC+buT9BxX77W7cGP97akH4unvW+lgtyagiIUj+ArD0EKvvaPjcLtcZPCfU+pMJSGAH1lr+uLUHo8P3vvwAAAAAAAAAA6hI11QCsxr6vzrMhXAunP3jdFcCy9+8/Aay8s7GXEOi+bZE+n4REtz7VtWt1W9n2Phqyp5WcgbrALqeCeHklg8A5IgZhP9aiv4NPCayU6uU/w9rONlWzHsCUIuwDMq82P43qZTMDTO1AcuUByTpXHj8=\", \"time\": 60000.100024183375, \"time_step\": 0.00011574074074074072}, \"event\": \"enqueue\", \"stream_id\": \"PTnSrPdY4f8c2XSBcZX9LH.gnc\"}";
      let entry: FileTargetFrameEntry = serde_json::from_str(d).unwrap();
      assert_eq!(entry.data.time, 60000.100024183375);
    }
}
