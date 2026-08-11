use std::{path::PathBuf, process::Stdio};

#[cfg(feature = "resource-metrics")]
use std::env::var;

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, BufReader, BufWriter},
    process, select,
    sync::mpsc,
    task::JoinHandle,
};
use tracing::{Instrument, debug, error, info, trace, warn};

#[cfg(feature = "resource-metrics")]
use super::observability::metrics_handler;
use crate::AutonomyModeId;

const SANDBOX_STOP_SIGNAL: &str = "__SAFE_STOP__";

#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub resources: SandboxResources,
    pub persist_work_dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxResources {
    pub cpu: f64,
    pub memory: u64,
    pub disk: u64,
}

#[allow(unused)]
pub trait Sandbox {
    fn new(name: AutonomyModeId, config: SandboxConfig, base_work_dir: Option<PathBuf>) -> Self;

    fn start(
        &mut self,
        override_args: Option<Vec<String>>,
    ) -> impl Future<Output = std::io::Result<JoinHandle<()>>>;

    fn get_work_dir(&self) -> &PathBuf;

    fn persist_work_dir(&self) -> bool;

    fn stop(&self) -> impl Future<Output = std::io::Result<()>>;
}

pub struct SafeSandbox {
    id: AutonomyModeId,
    config: SandboxConfig,
    killshot: Option<mpsc::Sender<String>>,
    work_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SandboxIsolationPolicy {
    Auto,
    Required,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SandboxLaunchMode {
    Namespaced,
    Direct,
}

#[cfg(target_os = "linux")]
fn build_namespaced_command(command: PathBuf, args: Vec<String>) -> process::Command {
    let mut sandbox_command = process::Command::new("unshare");
    sandbox_command
        .arg("--mount")
        .arg("--pid")
        .arg("--fork")
        .arg("--mount-proc")
        .arg("--")
        .arg(command)
        .args(args);

    sandbox_command
}

fn build_direct_command(command: PathBuf, args: Vec<String>) -> process::Command {
    let mut sandbox_command = process::Command::new(command);
    sandbox_command.args(args);
    sandbox_command
}

fn sandbox_isolation_policy() -> SandboxIsolationPolicy {
    match std::env::var("SAFE_SANDBOX_ISOLATION") {
        Ok(v) => match v.to_lowercase().as_str() {
            "required" => SandboxIsolationPolicy::Required,
            "disabled" => SandboxIsolationPolicy::Disabled,
            _ => SandboxIsolationPolicy::Auto,
        },
        Err(_) => SandboxIsolationPolicy::Auto,
    }
}

fn sandbox_max_restarts() -> u32 {
    std::env::var("SAFE_SANDBOX_MAX_RESTARTS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(5)
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                let _ = chars.next();
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
                continue;
            }
        }
        out.push(ch);
    }
    out
}

fn detected_mode_level(line: &str) -> Option<&'static str> {
    let mut tokens = line.split_whitespace();
    let first = tokens.next()?;
    let level = |token| match token {
        "TRACE" => Some("TRACE"),
        "DEBUG" => Some("DEBUG"),
        "INFO" => Some("INFO"),
        "WARN" | "WARNING" => Some("WARN"),
        "ERROR" => Some("ERROR"),
        _ => None,
    };
    if let Some(level) = level(first) {
        return Some(level);
    }

    let looks_like_timestamp = first.contains('T')
        || (first.len() >= 10
            && first.as_bytes().get(4) == Some(&b'-')
            && first.as_bytes().get(7) == Some(&b'-'));
    if looks_like_timestamp {
        return tokens.take(3).find_map(level);
    }
    None
}

fn log_mode_line(id: &str, stream: &str, line: &str, default_stderr: bool) {
    let line = strip_ansi(line);
    let level = detected_mode_level(&line).unwrap_or(if default_stderr { "WARN" } else { "INFO" });
    match level {
        "ERROR" => error!(mode_id = %id, stream = %stream, "{line}"),
        "WARN" => warn!(mode_id = %id, stream = %stream, "{line}"),
        "DEBUG" => debug!(mode_id = %id, stream = %stream, "{line}"),
        "TRACE" => trace!(mode_id = %id, stream = %stream, "{line}"),
        _ => info!(mode_id = %id, stream = %stream, "{line}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_isolation_policy_values() {
        unsafe {
            std::env::set_var("SAFE_SANDBOX_ISOLATION", "required");
        }
        assert_eq!(sandbox_isolation_policy(), SandboxIsolationPolicy::Required);

        unsafe {
            std::env::set_var("SAFE_SANDBOX_ISOLATION", "disabled");
        }
        assert_eq!(sandbox_isolation_policy(), SandboxIsolationPolicy::Disabled);

        unsafe {
            std::env::set_var("SAFE_SANDBOX_ISOLATION", "auto");
        }
        assert_eq!(sandbox_isolation_policy(), SandboxIsolationPolicy::Auto);
    }

    #[test]
    fn parses_max_restart_value_or_default() {
        unsafe {
            std::env::set_var("SAFE_SANDBOX_MAX_RESTARTS", "9");
        }
        assert_eq!(sandbox_max_restarts(), 9);

        unsafe {
            std::env::set_var("SAFE_SANDBOX_MAX_RESTARTS", "not-a-number");
        }
        assert_eq!(sandbox_max_restarts(), 5);
    }

    #[test]
    fn strip_ansi_removes_escape_sequences() {
        let s = "\u{1b}[32mINFO\u{1b}[0m hello";
        assert_eq!(strip_ansi(s), "INFO hello");
    }

    #[test]
    fn detects_level_only_in_the_prefix() {
        assert_eq!(
            detected_mode_level("2026-01-01T00:00:00Z ERROR target: failed"),
            Some("ERROR")
        );
        assert_eq!(
            detected_mode_level("2026-01-01 00:00:00 ERROR target: failed"),
            Some("ERROR")
        );
        assert_eq!(
            detected_mode_level("message contains ERROR but has no level prefix"),
            None
        );
    }
}

#[cfg(target_os = "linux")]
async fn supports_namespace_isolation() -> bool {
    match process::Command::new("unshare")
        .arg("--mount")
        .arg("--pid")
        .arg("--fork")
        .arg("--mount-proc")
        .arg("--")
        .arg("true")
        .output()
        .await
    {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

#[cfg(not(target_os = "linux"))]
async fn supports_namespace_isolation() -> bool {
    false
}

fn build_sandbox_command(
    launch_mode: SandboxLaunchMode,
    command: PathBuf,
    args: Vec<String>,
) -> process::Command {
    match launch_mode {
        SandboxLaunchMode::Namespaced => {
            #[cfg(target_os = "linux")]
            {
                build_namespaced_command(command, args)
            }
            #[cfg(not(target_os = "linux"))]
            {
                build_direct_command(command, args)
            }
        }
        SandboxLaunchMode::Direct => build_direct_command(command, args),
    }
}

impl Sandbox for SafeSandbox {
    fn new(id: AutonomyModeId, config: SandboxConfig, base_work_dir: Option<PathBuf>) -> Self {
        let work_dir = if let Some(base_work_dir) = base_work_dir {
            base_work_dir
        } else {
            std::env::temp_dir()
                .join("safe-sandbox")
                .join(id.to_string())
        };
        info!("Work dir: {work_dir:?}");
        if !work_dir.exists() {
            error!("Work dir was not created for sandbox: {work_dir:?}");
        }

        SafeSandbox {
            id,
            config,
            killshot: None,
            work_dir,
        }
    }

    fn get_work_dir(&self) -> &PathBuf {
        &self.work_dir
    }

    fn persist_work_dir(&self) -> bool {
        self.config.persist_work_dir
    }

    async fn start(
        &mut self,
        override_args: Option<Vec<String>>,
    ) -> std::io::Result<JoinHandle<()>> {
        let command = self.config.command.clone();
        let args = self.config.args.clone();

        // Configurable via env var
        let work_dir = self.get_work_dir().clone();

        let (killshot_tx, mut killshot_rx) = mpsc::channel(1);
        #[cfg(feature = "resource-metrics")]
        let resource_watcher_killshot_tx = killshot_tx.clone();
        self.killshot = Some(killshot_tx);

        let child_uuid = self.id;
        #[cfg(feature = "resource-metrics")]
        let child_resources = self.config.resources.clone();

        info!("New sandbox {child_uuid:?}");

        let args = if let Some(override_args) = override_args {
            override_args
        } else {
            args
        };

        let id = self.id.clone();
        let isolation_policy = sandbox_isolation_policy();
        let max_restarts = sandbox_max_restarts();
        let launch_mode = match isolation_policy {
            SandboxIsolationPolicy::Disabled => SandboxLaunchMode::Direct,
            SandboxIsolationPolicy::Auto => {
                if supports_namespace_isolation().await {
                    SandboxLaunchMode::Namespaced
                } else {
                    info!("Sandbox namespace isolation unavailable; falling back to direct launch");
                    SandboxLaunchMode::Direct
                }
            }
            SandboxIsolationPolicy::Required => {
                if supports_namespace_isolation().await {
                    SandboxLaunchMode::Namespaced
                } else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "sandbox namespace isolation required but unavailable",
                    ));
                }
            }
        };

        let event_loop = tokio::spawn(async move {
            let mut command = build_sandbox_command(launch_mode, command, args);
            let child = command
                .current_dir(&work_dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            let mut restart_count: u32 = 0;

            while let Ok(mut child) = child.spawn() {
                let mut stop_requested = false;
                let stdin = child.stdin.take().unwrap();
                let stdout = child.stdout.take().unwrap();
                let stderr = child.stderr.take().unwrap();

                // Spawn off a tokio task to handle stdio reading and writing
                let id = id.clone().to_string();
                tokio::spawn(async move {
                    let mut stdout_reader = BufReader::new(stdout).lines();
                    let mut stderr_reader = BufReader::new(stderr).lines();
                    let _ = BufWriter::new(stdin); // TODO: stdin
                    let mut stdout_open = true;
                    let mut stderr_open = true;

                    while stdout_open || stderr_open {
                        select! {
                            res = stdout_reader.next_line(), if stdout_open => {
                                match res {
                                    Ok(res) => {
                                        match res {
                                            Some(line) => {
                                                log_mode_line(&id, "stdout", &line, false);
                                            }
                                            None => {
                                                stdout_open = false;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!(stream = "stdout", "mode stdout read failed: {e}");
                                        stdout_open = false;
                                    }
                                }
                            }
                            res = stderr_reader.next_line(), if stderr_open => {
                                match res {
                                    Ok(res) => {
                                        match res {
                                            Some(line) => {
                                                log_mode_line(&id, "stderr", &line, true);
                                            }
                                            None => {
                                                stderr_open = false;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!(stream = "stderr", "mode stderr read failed: {e}");
                                        stderr_open = false;
                                    }
                                }
                            }
                        }
                    }
                }
                .in_current_span());

                #[cfg(feature = "resource-metrics")]
                {
                    let (resource_tx, mut resource_rx) = mpsc::channel::<(f64, u64, u64)>(10);

                    let child_id = child.id().unwrap() as usize;
                    let hardcoded_writable_dir: String = var("SAFE_METRIC_BASE_PATH")
                        .unwrap_or_else(|_| "/tmp/safe".into());
                    let hardcoded_writable_dir = PathBuf::from(hardcoded_writable_dir);
                    let work_dir = hardcoded_writable_dir.join(child_uuid.0.to_string());
                    tokio::spawn(async move {
                        metrics_handler(child_id, child_uuid.0, resource_tx, work_dir).await;
                    }
                    .in_current_span());

                    let resource_watcher_killshot_tx = resource_watcher_killshot_tx.clone();
                    tokio::spawn(async move {
                        while let Some(msg) = resource_rx.recv().await {
                            if msg.0 > child_resources.cpu {
                                resource_watcher_killshot_tx
                                    .send("CPU threshold".into())
                                    .await
                                    .unwrap();
                            } else if msg.1 > child_resources.memory {
                                resource_watcher_killshot_tx
                                    .send("Memory threshold".into())
                                    .await
                                    .unwrap();
                            } else if msg.2 > child_resources.disk {
                                resource_watcher_killshot_tx
                                    .send("Disk write threshold".into())
                                    .await
                                    .unwrap();
                            }
                        }
                    }
                    .in_current_span());
                }

                // This waits for either the sandbox to exit by itself OR a
                // killshot comes on the channel for which we send SIGKILL
                let child_res = select! {
                    res = child.wait() => {
                        res
                    }
                    msg = killshot_rx.recv() => {
                        if msg.is_some() {
                            let msg = msg.unwrap();
                            stop_requested = msg == SANDBOX_STOP_SIGNAL;
                            info!("SEND SIGKILL - msg: {msg}");
                            match child.kill().await {
                                Ok(_) => {
                                    info!("KILLED");
                                    child.wait().await
                                }
                                Err(e) => {
                                    error!("DID NOT KILL: {e}");
                                    child.wait().await
                                }
                            }
                        } else {
                            child.wait().await
                        }
                    }
                };

                // Finally, if we error, we restart the sandbox with the same
                // unique ID as it was given at start
                if stop_requested {
                    info!("Sandbox stop requested; exiting supervisor loop");
                    break;
                }

                match child_res {
                    Ok(exit_status) => {
                        if !exit_status.success() {
                            restart_count += 1;
                            if restart_count > max_restarts {
                                error!(
                                    "Failed with status: {exit_status}. Restart limit exceeded ({max_restarts}); stopping sandbox"
                                );
                                break;
                            }
                            error!(
                                "Failed with status: {exit_status}. Restarting ({restart_count}/{max_restarts})"
                            );
                        } else {
                            info!("Exited successfully");
                            break;
                        }
                    }
                    Err(e) => {
                        restart_count += 1;
                        if restart_count > max_restarts {
                            error!(
                                "Failed with error: {e}. Restart limit exceeded ({max_restarts}); stopping sandbox"
                            );
                            break;
                        }
                        error!(
                            "Failed with error: {e}. Restarting ({restart_count}/{max_restarts})"
                        );
                    }
                }
            }
        }
        .in_current_span());

        Ok(event_loop)
    }

    async fn stop(&self) -> std::io::Result<()> {
        info!("Stopping sandbox");
        self.killshot
            .clone()
            .unwrap()
            .send(SANDBOX_STOP_SIGNAL.into())
            .await
            .expect("stopping from stop method");
        Ok(())
    }
}
