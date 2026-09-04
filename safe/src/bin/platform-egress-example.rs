use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use safe::protocol::{BoardCmdId, BoardState, TimedCommand};
use safe::runtime::HostCommandStatus;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum EgressInput {
    HostCommandStatus { status: HostCommandStatus },
    BoardSnapshot { board: BoardState },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum EgressOutput {
    BoardPublished { command_ids: Vec<BoardCmdId> },
}

#[derive(Serialize)]
struct CommandCsvRecord {
    cmd: String,
    gps_time: f64,
}

fn main() -> anyhow::Result<()> {
    let base_path = parse_base_path()?;
    let status_path = base_path.join("state/host_command_status.jsonl");
    let commands_path = base_path.join("out/commands.csv");
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();

    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let input = match serde_json::from_str::<EgressInput>(trimmed) {
            Ok(input) => input,
            Err(e) => {
                eprintln!("invalid platform egress input: {e}; line={trimmed}");
                continue;
            }
        };

        match input {
            EgressInput::HostCommandStatus { status } => append_status(&status_path, &status)?,
            EgressInput::BoardSnapshot { board } => {
                write_commands_csv(&commands_path, &board)?;
                serde_json::to_writer(
                    &mut stdout,
                    &EgressOutput::BoardPublished {
                        command_ids: board.source_of_truth,
                    },
                )?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
        }
    }

    Ok(())
}

fn parse_base_path() -> anyhow::Result<PathBuf> {
    let mut args = std::env::args().skip(1);
    let mut base_path = PathBuf::from("/tmp/safe");

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--base-path" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--base-path requires a value"))?;
                base_path = PathBuf::from(value);
            }
            "--help" | "-h" => {
                println!("Usage: platform-egress-example [--base-path PATH]");
                std::process::exit(0);
            }
            _ => anyhow::bail!("unknown argument: {arg}"),
        }
    }

    Ok(base_path)
}

fn append_status(path: &Path, status: &HostCommandStatus) -> anyhow::Result<()> {
    create_parent(path)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    serde_json::to_writer(&mut file, status)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

fn write_commands_csv(path: &Path, board: &BoardState) -> anyhow::Result<()> {
    create_parent(path)?;
    let mut commands = board
        .source_of_truth
        .iter()
        .filter_map(|id| board.proposals.get(id))
        .filter_map(|(_, command, _)| match command {
            TimedCommand::Scheduled { cmd, gps_time } => Some(CommandCsvRecord {
                cmd: cmd.clone().into(),
                gps_time: *gps_time,
            }),
            TimedCommand::Now(_) | TimedCommand::NOOP => None,
        })
        .collect::<Vec<_>>();
    commands.sort_by(|a, b| {
        a.gps_time
            .partial_cmp(&b.gps_time)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let tmp_path = path.with_extension("csv.tmp");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_path)?;
    let mut writer = csv::Writer::from_writer(file);
    for command in commands {
        writer.serialize(command)?;
    }
    writer.flush()?;
    std::fs::rename(tmp_path, path)?;
    Ok(())
}

fn create_parent(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}
