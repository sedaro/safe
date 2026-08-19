#![doc = include_str!("../README.md")]

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::SeekFrom;
use std::io::{BufRead, BufReader, Seek};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use serde::{Deserialize, Serialize};
use simvm::sv::ser_de::{dyn_de, dyn_ser};
use simvm::sv::{combine::TR, combine::TRD, parse::Parse};
use tokio::{process::Command as TokioCommand, time::timeout};

pub mod study;

pub use study::{
    EdsPatchTarget, MonteCarloParameter, MonteCarloStudy, ProbabilityDistribution, StudyCase,
    StudyResult, StudyRunFailure, StudyRunOutcome, StudyRunResult, StudyTermination, TradeStudy,
};
pub use tokio_util::sync::CancellationToken;

const SAFE_DELETE_EDS_OUTPUT_FILES_ENV: &str = "SAFE_DELETE_EDS_OUTPUT_FILES";

fn should_delete_eds_output_files() -> bool {
    std::env::var(SAFE_DELETE_EDS_OUTPUT_FILES_ENV)
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "false" | "no" | "off")
        })
        .unwrap_or(true)
}
/// The process output and decoded target frames produced by one simulation.
#[derive(Clone, Debug, Default)]
pub struct SimulationResult {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub frames_by_file: HashMap<String, Vec<TRD>>,
}

impl SimulationResult {
    /// Collects decoded JSONL target frames and process output from an EDS workspace.
    pub fn from_target_dir(
        output: &std::process::Output,
        target_dir: &std::path::Path,
    ) -> anyhow::Result<Self> {
        tracing::debug!(
            "Collecting simulation results from target directory '{}'",
            target_dir.display()
        );

        if !output.status.success() {
            tracing::warn!(
                "Simulation reported failure before output collection (exit_code={:?}); attempting to collect any available result files from '{}'",
                output.status.code(),
                target_dir.display()
            );
        }

        fn collect_jsonl_paths_recursive(
            dir: &std::path::Path,
            out: &mut Vec<std::path::PathBuf>,
            seen: &mut HashSet<std::path::PathBuf>,
        ) -> anyhow::Result<()> {
            if !dir.exists() || !dir.is_dir() {
                return Ok(());
            }

            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    collect_jsonl_paths_recursive(&path, out, seen)?;
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    let canonical = path.canonicalize().unwrap_or(path);
                    if seen.insert(canonical.clone()) {
                        out.push(canonical);
                    }
                }
            }
            Ok(())
        }

        let mut frames_by_file: HashMap<String, Vec<TRD>> = HashMap::new();
        let mut jsonl_paths = Vec::new();
        let mut seen = HashSet::new();

        let mut candidate_dirs = vec![
            target_dir.to_path_buf(),
            target_dir.join("local").join("output"),
            target_dir.join("output"),
        ];
        if let Some(parent) = target_dir.parent() {
            candidate_dirs.push(parent.join("local").join("output"));
            candidate_dirs.push(parent.join("output"));
        }

        for dir in candidate_dirs {
            collect_jsonl_paths_recursive(&dir, &mut jsonl_paths, &mut seen)?;
        }

        if jsonl_paths.is_empty() {
            tracing::warn!(
                "No .jsonl simulation output files found under target '{}', '{}/local/output', or sibling output directories",
                target_dir.display(),
                target_dir.display()
            );
        }

        jsonl_paths.sort();
        for path in &jsonl_paths {
            let mut reader = FileTargetReader::try_from_path(path).with_context(|| {
                format!("failed to open simulation output file '{}'", path.display())
            })?;
            let frames = reader.read_frames().with_context(|| {
                format!(
                    "failed to parse simulation output file '{}'",
                    path.display()
                )
            })?;
            let key = path
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| "unknown.jsonl".to_string());
            if frames_by_file.insert(key.clone(), frames).is_some() {
                anyhow::bail!("multiple simulation output files have the same name '{key}'");
            }
        }

        tracing::debug!(
            "Collected {} frames across {} files",
            frames_by_file.values().map(|v| v.len()).sum::<usize>(),
            frames_by_file.len()
        );

        // once frames have been read, delete the JSONL files to save disk space
        if should_delete_eds_output_files() {
            for path in &jsonl_paths {
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::warn!(
                        "Failed to delete simulation output file '{}': {:?}",
                        path.display(),
                        e
                    );
                }
            }
        } else {
            tracing::debug!(
                "Keeping {} simulation output .jsonl files in place ({}=false)",
                jsonl_paths.len(),
                SAFE_DELETE_EDS_OUTPUT_FILES_ENV
            );
        }

        Ok(Self {
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            frames_by_file,
        })
    }

    /// Returns the total number of decoded frames across all target files.
    pub fn total_frames(&self) -> usize {
        self.frames_by_file.values().map(|v| v.len()).sum()
    }
}

/// Configures and launches simulations from an EDS workspace.
#[derive(Debug, Serialize, Clone)]
pub struct SedaroSimulator {
    path: std::path::PathBuf,
    args: Vec<String>,
    timeout: Duration,
    venv: Option<std::path::PathBuf>,
    init_type: Option<TR>,
    epoch: Option<f64>,
}

/// Describes one EDS initial-state patch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdsPatch {
    /// The ID of the agent where the patched field exists
    pub agent_id: String,

    /// The simulation engine where the patched field is produced
    pub engine: String,

    /// The field to patch, specified as a dot-separated path (e.g. "block_id.var")
    pub field: String,

    /// The type of the patched field, serialized as a string (e.g. "f64", "string", or a more complex type signature)
    pub type_: String,

    /// The value to patch, serialized as a string
    /// Note: string types must be double-quoted in the value (e.g. "\"patched string\"")
    pub value: String,
}

impl EdsPatch {
    /// Creates a patch for one field in an agent simulation engine.
    pub fn new(agent_id: &str, engine: &str, field: &str, type_: &str, value: &str) -> Self {
        EdsPatch {
            agent_id: agent_id.to_string(),
            engine: engine.to_string(),
            field: field.to_string(),
            type_: type_.to_string(),
            value: value.to_string(),
        }
    }

    fn to_tuple(&self) -> (String, String) {
        (
            format!(
                r#"({}: ({}: ('{}': {},),),)"#,
                self.agent_id, self.engine, self.field, self.type_
            ),
            if self.type_ == "str" {
                format!("((('{}',),),)", self.value)
            } else {
                format!("((({},),),)", self.value)
            },
        )
    }
}

impl SedaroSimulator {
    /// Creates a simulator from an EDS executable or workspace path.
    pub fn new(path: &std::path::PathBuf) -> Self {
        SedaroSimulator {
            path: path.clone(),
            args: Vec::new(),
            timeout: Duration::MAX,
            venv: None,
            init_type: None,
            epoch: None,
        }
    }

    /// Sets the simulation start epoch in modified Julian days.
    pub fn at_epoch(mut self, epoch_mjd: f64) -> Self {
        self.epoch = Some(epoch_mjd);
        self
    }

    /// Resolves the EDS workspace and executable from the configured path.
    fn resolve_workspace_and_executable(&self) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
        let canonical_path = self.path.canonicalize().with_context(|| {
            format!("failed to canonicalize eds_path '{}'", self.path.display())
        })?;

        if canonical_path.is_file() {
            let workspace_dir = canonical_path
                .parent()
                .map(|p| p.to_path_buf())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "eds executable path '{}' has no parent directory",
                        canonical_path.display()
                    )
                })?;
            return Ok((workspace_dir, canonical_path));
        }

        if canonical_path.is_dir() {
            let executable_path = canonical_path.join("eds");
            if executable_path.is_file() {
                return Ok((canonical_path, executable_path));
            }

            anyhow::bail!(
                "eds_path '{}' is a directory but no executable was found at '{}'",
                canonical_path.display(),
                executable_path.display()
            );
        }

        anyhow::bail!(
            "eds_path '{}' must be a file or directory",
            canonical_path.display()
        );
    }

    /// Returns the workspace containing the configured EDS executable.
    pub fn workspace_dir(&self) -> Result<std::path::PathBuf> {
        let (workspace_dir, _) = self.resolve_workspace_and_executable()?;
        Ok(workspace_dir)
    }

    /// Appends raw command-line arguments passed to EDS.
    pub fn args(mut self, args: Vec<&str>) -> Self {
        self.args.extend(args.iter().map(|s| s.to_string()));
        self
    }

    /// Appends one initial-state patch to the EDS command line.
    pub fn patch(mut self, patch: EdsPatch) -> Self {
        self.args.push("--patch".to_string());
        let parts = patch.to_tuple();
        self.args.push(parts.0);
        self.args.push(parts.1);
        self
    }

    /// Appends multiple initial-state patches to the EDS command line.
    pub fn patch_multi(mut self, patches: Vec<EdsPatch>) -> Self {
        for patch in patches {
            self = self.patch(patch);
        }
        self
    }

    /// Sets the maximum wall-clock duration of one simulation process.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Runs EDS for the requested number of simulation days.
    pub async fn run(&self, duration_days: f64) -> Result<std::process::Output> {
        let (workspace_dir, executable_path) = self.resolve_workspace_and_executable()?;
        let mut command_args = vec!["--duration".to_string(), duration_days.to_string()];
        if let Some(epoch_mjd) = self.epoch {
            command_args.push("--start".to_string());
            command_args.push(epoch_mjd.to_string());
        }

        let mut cmd = TokioCommand::new(&executable_path);
        let cmd = cmd
            .args(command_args)
            .args(self.args.clone())
            .current_dir(&workspace_dir)
            .kill_on_drop(true);
        tracing::debug!("Running command: {:?}", &cmd);

        match timeout(self.timeout, cmd.output()).await {
            Err(_) => {
                tracing::warn!(
                    "Simulation command timed out after {} seconds (workspace='{}', executable='{}')",
                    self.timeout.as_secs(),
                    workspace_dir.display(),
                    executable_path.display()
                );
                Err(anyhow::anyhow!(
                    "Simulation timed out after {:?} seconds",
                    self.timeout.as_secs()
                ))
            }
            Ok(output) => {
                let output = match output.with_context(|| {
                    format!(
                        "failed to execute eds executable '{}' from workspace '{}'",
                        executable_path.display(),
                        workspace_dir.display()
                    )
                }) {
                    Ok(output) => output,
                    Err(e) => {
                        tracing::warn!(
                            "Simulation command execution failed (workspace='{}', executable='{}'): {e:#}",
                            workspace_dir.display(),
                            executable_path.display()
                        );
                        return Err(e);
                    }
                };

                if !output.status.success() {
                    tracing::warn!(
                        "Simulation command exited with non-zero status (exit_code={:?}, workspace='{}', executable='{}', stdout='{}', stderr='{}')",
                        output.status.code(),
                        workspace_dir.display(),
                        executable_path.display(),
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }

                Ok(output)
            }
        }
    }

    /// Runs EDS and collects its decoded target files into a simulation result.
    pub async fn run_collect(&self, duration_days: f64) -> Result<SimulationResult> {
        let workspace_dir = self.workspace_dir()?;
        let output = self.run(duration_days).await?;
        SimulationResult::from_target_dir(&output, &workspace_dir)
    }

    /// Reads the serialized type definition for an agent's initial state.
    pub fn read_init_type(&self, agent_id: &str) -> Result<TR> {
        let workspace_dir = self.workspace_dir()?;
        match &self.init_type {
            Some(ty) => Ok(ty.clone()),
            None => {
                let type_sig =
                    std::fs::read(workspace_dir.join(format!("data/init_ty_{agent_id}.json")))?;
                let type_sig_str = std::str::from_utf8(&type_sig)?;
                let parsed_ty = TR::parse(type_sig_str).unwrap();
                Ok(parsed_ty)
            }
        }
    }

    /// Reads and decodes an agent's initial-state value.
    pub fn read_init_trd(&self, agent_id: &str) -> Result<TRD> {
        let init_type = self.read_init_type(agent_id)?;
        let workspace_dir = self.workspace_dir()?;
        let init_file_path = workspace_dir
            .join(format!("data/init_{agent_id}.bin"))
            .canonicalize()?;
        let init_bytes = std::fs::read(&init_file_path)?;
        let init_val = dyn_de(&init_type.typ, &init_bytes).unwrap();
        let init_val = TRD::from((init_type.clone(), init_val));
        Ok(init_val)
    }

    /// Encodes and writes an agent's initial-state value.
    pub fn write_init_trd(&self, agent_id: &str, init_val: TRD) -> Result<()> {
        let init_type = self.read_init_type(agent_id)?;
        let workspace_dir = self.workspace_dir()?;
        let init_file_path = workspace_dir
            .join(format!("data/init_{agent_id}.bin"))
            .canonicalize()?;
        let init_val = init_val.data;
        let bytes = dyn_ser(&init_type.typ, &init_val).unwrap();
        std::fs::write(&init_file_path, bytes)?;
        Ok(())
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
    frames: Vec<TRD>,
    line_idx: u64,
}

impl FileTargetReader {
    /// Opens an EDS JSONL target file for incremental frame reads.
    pub fn try_from_path(target_file_path: &std::path::PathBuf) -> Result<Self> {
        let path = target_file_path.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize simulation output file path '{}'",
                target_file_path.display()
            )
        })?;
        let file = File::open(&path).with_context(|| {
            format!("failed to open simulation output file '{}'", path.display())
        })?;
        Ok(FileTargetReader {
            reader: BufReader::new(file),
            ty: None,
            timestamps_mjd: vec![],
            frames: vec![],
            line_idx: 0,
        })
    }

    /// Parses target entries added since the previous read.
    fn parse_frames(&mut self) -> Result<()> {
        self.reader.seek(SeekFrom::Start(self.line_idx))?;
        loop {
            let mut line = String::new();
            let bytes_read = self.reader.read_line(&mut line)?;
            if bytes_read == 0 {
                break; // Reached end of file
            }
            if self.ty.is_none() {
                if let Ok(config) = serde_json::from_str::<FileTargetConfigEntry>(&line) {
                    self.ty = Some(TR::parse(&config.data.type_).map_err(|error| {
                        anyhow::anyhow!("invalid target type '{}': {error:?}", config.data.type_)
                    })?);
                }
            } else if let Ok(entry) = serde_json::from_str::<FileTargetFrameEntry>(&line) {
                if let Some(parsed) = &self.ty {
                    let frame_bytes = BASE64_STANDARD
                        .decode(&entry.data.frame)
                        .context("invalid base64 target frame")?;
                    match dyn_de(&parsed.typ, &frame_bytes) {
                        Ok(val) => {
                            self.timestamps_mjd.push(entry.data.time);
                            self.frames.push(TRD::from((parsed.clone(), val))); // TODO: More memory efficient approach?
                        }
                        Err(e) => {
                            return Err(anyhow::anyhow!(
                                "Simulation frame deserialization error: {:?}",
                                e
                            ));
                        }
                    }
                }
            }
            self.line_idx += 1;
        }
        Ok(())
    }

    /// Returns frames added since the previous call.
    pub fn read_frames(&mut self) -> Result<Vec<TRD>> {
        self.parse_frames()?;
        let frames = std::mem::take(&mut self.frames);
        Ok(frames)
    }

    /// Finds the insertion index of a target timestamp.
    fn idx_of_timestamp(&self, timestamp_mjd: f64) -> usize {
        match self
            .timestamps_mjd
            .binary_search_by(|&probe| probe.partial_cmp(&timestamp_mjd).unwrap())
        {
            // TODO: Write test for edge cases of search
            Ok(idx) => idx,
            Err(idx) => idx,
        }
    }
    /// Returns the first frame at or after a modified Julian timestamp.
    pub fn read_frame_at_timestamp(&mut self, timestamp_mjd: f64) -> Result<Option<TRD>> {
        self.parse_frames()?;
        if let Some(start_time_mjd) = self.timestamps_mjd.first() {
            if timestamp_mjd < *start_time_mjd
                || timestamp_mjd > *self.timestamps_mjd.last().unwrap()
            {
                return Ok(None);
            }
        } else {
            return Ok(None);
        }
        Ok(Some(
            self.frames[self.idx_of_timestamp(timestamp_mjd)].clone(),
        ))
    }
    /// Returns the frame at an elapsed duration from the first target frame.
    pub fn read_frame_at_elapsed(&mut self, duration: Duration) -> Result<Option<TRD>> {
        self.parse_frames()?; // TODO: Figure out how to avoid this redundant parse
        if let Some(start_time_mjd) = self.timestamps_mjd.first() {
            let timestamp_mjd = start_time_mjd + (duration.as_secs_f64() / 86400.0);
            self.read_frame_at_timestamp(timestamp_mjd)
        } else {
            println!("No frames available to read at elapsed time {:?}", duration);
            Ok(None)
        }
    }
}

// TODO: Implement transpose?

#[cfg(test)]
mod tests {
    use crate::FileTargetFrameEntry;

    #[test]
    fn test_deser() {
        let d = "{\"data\": {\"frame\": \"Ojh7A1M4aD4OXAPUmHGxPzo4ewNTOGg+AAAAAAAAAAAAAAAAAAAAAN2GE2yw0FW+c/MadAWsor8AAAAAAAAAgN2GE2yw0FW+AAAAAAAAAIB8vJBmDtUhPn7HaQ7qt82/AAAAAAAAAAAAAAAAAAAAAHy8kGYO1SE+3Lw5s5ZdBL/cvDmzll0kvwAAAAAAAACAabUM7FgVDT7TqOkwv53NvW4PIvlgaBk/bg8i+WBoOT8Axu8aVyQiPgAAAAAAAACAQcL3XHUbEz6Rzddv4er7vpHN12/h6hu/x1lA25hMxD1Ai/nbov70PQAAAAAAAACA8txmVC+buT9BxX77W7cGP97akH4unvW+lgtyagiIUj+ArD0EKvvaPjcLtcZPCfU+pMJSGAH1lr+uLUHo8P3vvwAAAAAAAAAA6hI11QCsxr6vzrMhXAunP3jdFcCy9+8/Aay8s7GXEOi+bZE+n4REtz7VtWt1W9n2Phqyp5WcgbrALqeCeHklg8A5IgZhP9aiv4NPCayU6uU/w9rONlWzHsCUIuwDMq82P43qZTMDTO1AcuUByTpXHj8=\", \"time\": 60000.100024183375, \"time_step\": 0.00011574074074074072}, \"event\": \"enqueue\", \"stream_id\": \"PTnSrPdY4f8c2XSBcZX9LH.gnc\"}";
        let entry: FileTargetFrameEntry = serde_json::from_str(d).unwrap();
        assert_eq!(entry.data.time, 60000.100024183375);
    }
}
