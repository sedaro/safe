use std::thread::sleep;
use std::time::Duration;

use clap::{CommandFactory, Parser, Subcommand};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::net::UnixStream;
use tokio_util::codec::Framed;
use tokio_util::codec::LengthDelimitedCodec;

const SOCKET_PATH: &str = "/tmp/safe.sock";

// TOOD: Reference these from safe-common crate
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Telemetry {
    pub timestamp: u64,
    pub proximity_m: i32,
}
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct LogsRequest {
    pub mode: Option<String>,
    pub level: Option<String>,
    pub follow: bool,
}
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct CommandsRequest {}

/*
CLI
- get mode(s) <mode> -o
- describe mode(s) <mode> <mode>
- get router -o
- logs -m <mode> | -r, --tail, -f, --since, --before, --filter, --level
- top
- tx
- rx
-- needs more thoughts
- config -f <file>
- config set <variable>
- config

Old:
- safectl get modes -A -w
- safectl get modes -m <mode name> -w
- safectl logs -A -f
- safectl logs -m <mode name> -f --tail --since --before --filter
- safectl top modes -A
- safectl top modes -m <mode name> -w
- safectl top
= safectl config
- safectl send
- safectl install/uninstall <file> | <raw>
- safectl debug
 */

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Turn debugging information on
    #[arg(short, long, short_alias = 'v', action = clap::ArgAction::Count)]
    debug: u8,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Object {
    Modes {
        /// Get all Autonomy Modes
        #[arg(short = 'A', long)]
        all: bool,

        /// Get specific Autonomy Mode by name
        #[arg(short, long)]
        name: Option<String>,

        /// Output format (TODO)
        #[arg(short, long)]
        output: Option<String>,
    },
    Router {
        /// Output format (TODO)
        #[arg(short, long)]
        output: Option<String>,
    },
}

#[derive(Subcommand)]
enum Commands {
    /// Get Autonomy Mode(s)
    Get {
        #[command(subcommand)]
        command: Object,
    },
    /// Describe Autonomy Mode(s)
    Describe {
        #[command(subcommand)]
        command: Object,
    },
    /// Top Autonomy Mode(s)
    Top {
        #[command(subcommand)]
        command: Option<Object>,
    },
    /// Get logs
    Logs {
        /// Get specific Autonomy Mode
        #[arg(short, long)]
        mode: Option<String>,

        /// Tail the last N lines
        #[arg(short, long)]
        tail: Option<u32>,

        /// Stream logs
        #[arg(short, long)]
        follow: Option<bool>,

        /// Query for logs since ISO 8601 timestamp
        since: Option<String>,

        /// Query for logs before ISO 8601 timestamp
        before: Option<String>,

        /// Filter returned logs (TODO)
        filter: Option<String>,

        /// Minimum log level
        /// e.g., debug, info, warning, error
        #[arg(short, long)]
        level: Option<String>,
    },
    /// Transmit over C2
    #[command(alias = "tx")]
    Transmit {
        /// JSON payload to send
        json: String,
    },
    /// Receive over C2
    #[command(alias = "rx")]
    Receive,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    // setup_logging(cli.debug);
    match &cli.command {
        Some(Commands::Get { command }) => match command {
            Object::Modes { all, name, .. } => {
                println!("{:?} {:?}", all, name);
            }
            Object::Router { .. } => {}
        },
        Some(Commands::Describe { command }) => match command {
            Object::Modes { all, name, .. } => {
                println!("{:?} {:?}", all, name);
            }
            Object::Router { .. } => {}
        },
        Some(Commands::Top { command }) => match command {
            Some(Object::Modes { all, name, .. }) => {
                println!("{:?} {:?}", all, name);
            }
            Some(Object::Router { .. }) => {}
            None => Cli::command().print_help().unwrap(),
        },
        Some(Commands::Logs {
            mode,
            tail,
            follow,
            since,
            before,
            filter,
            level,
        }) => {
            if tail.is_some() { println!("WARNING: --tail is not yet implemented") }
            if follow.unwrap_or(false) { println!("WARNING: --follow is not yet implemented") }
            if since.is_some() { println!("WARNING: --since is not yet implemented") }
            if before.is_some() { println!("WARNING: --before is not yet implemented") }
            if filter.is_some() { println!("WARNING: --filter is not yet implemented") }
            
            let stream = TcpStream::connect("127.0.0.1:8001").await?;
            // println!("Connected");
            let mut framed_stream = Framed::new(stream, LengthDelimitedCodec::new());
            let request = LogsRequest {
                mode: mode.clone(),
                level: level.clone(),
                follow: follow.unwrap_or(false),
            };
            let msg = serde_json::to_string(&request).unwrap();
            let msg = bincode::serialize(&msg).unwrap();
            framed_stream.send(msg.into()).await?;
            loop {
                let bytes = framed_stream
                    .next()
                    .await
                    .ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Connection closed")
                    })??;
                let msg: String = bincode::deserialize(&bytes).unwrap();
                println!("{}", msg);
            }
        }
        Some(Commands::Transmit { json }) => {
            // let stream = UnixStream::connect(SOCKET_PATH).await?;
            let stream = TcpStream::connect("127.0.0.1:8001").await?;
            // println!("Connected");
            let mut framed_stream = Framed::new(stream, LengthDelimitedCodec::new());

            let telemetry: Telemetry =
                serde_json::from_str(&json).expect("Failed to parse JSON string");
            let msg = serde_json::to_string(&telemetry).unwrap();
            let msg = bincode::serialize(&msg).unwrap();
            framed_stream.send(msg.into()).await?;
        }
        Some(Commands::Receive {}) => {
            // let stream = UnixStream::connect(SOCKET_PATH).await?;
            let stream = TcpStream::connect("127.0.0.1:8001").await?;
            // println!("Connected");
            let mut framed_stream = Framed::new(stream, LengthDelimitedCodec::new());
            let request = CommandsRequest {};
            let msg = serde_json::to_string(&request).unwrap();
            let msg = bincode::serialize(&msg).unwrap();
            framed_stream.send(msg.into()).await?;
            loop {
                let bytes = framed_stream
                    .next()
                    .await
                    .ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Connection closed")
                    })??;
                let msg: String = bincode::deserialize(&bytes).unwrap();
                println!("{}", msg);
            }
        }
        None => Cli::command().print_help().unwrap(),
    }

    Ok(())
}
