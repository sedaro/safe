use std::os::unix::net::UnixStream;
use std::io::Write;
use std::io::Read;

use serde::{Deserialize, Serialize};
use std::env;
use clap::{CommandFactory, Parser, Subcommand};

const SOCKET_PATH: &str = "/tmp/safe.sock";

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Telemetry {
    pub timestamp: u64,
    pub proximity_m: i32,
}


/*
CLI
- get mode(s) <mode> -o
- describe mode(s) <mode> <mode>
- get router -o
- logs -m <mode> | -r, --tail, -f, --since, --before, --filter
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
  }
}

#[derive(Subcommand)]
enum Commands {
    /// Get Autonomy Mode(s)
    Get {
      #[command(subcommand)]
      command: Object
    },
    /// Describe Autonomy Mode(s)
    Describe {
      #[command(subcommand)]
      command: Object
    },
    /// Top Autonomy Mode(s)
    Top {
      #[command(subcommand)]
      command: Option<Object>,
    },
    /// Get logs
    Logs {
        /// Get all Autonomy Modes
        #[arg(short = 'A', long)]
        all: bool,
        
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

fn main() -> std::io::Result<()> {


    let cli = Cli::parse();
    // setup_logging(cli.debug);
    match &cli.command {
        Some(Commands::Get{ command }) => {
          match command {
            Object::Modes { all, name, .. } => {
              println!("{:?} {:?}", all, name);
            }
            Object::Router { .. } => {}
          }
        }
        Some(Commands::Describe{ command }) => {
          match command {
            Object::Modes { all, name, .. } => {
              println!("{:?} {:?}", all, name);
            }
            Object::Router { .. } => {}
          }
        }
        Some(Commands::Top{ command }) => {
          match command {
            Some(Object::Modes { all, name, .. }) => {
              println!("{:?} {:?}", all, name);
            }
            Some(Object::Router { .. }) => {}
            None => Cli::command().print_help().unwrap()
          }
        }
        Some(Commands::Logs { all, tail, follow, since, before, filter }) => {
          println!("{:?} {:?}", all, tail);
        }
        Some(Commands::Transmit { json }) => {
          let mut stream = UnixStream::connect(SOCKET_PATH)?;
          println!("Connected");
          
          let telemetry: Telemetry = serde_json::from_str(&json)
            .expect("Failed to parse JSON string");
          let mut msg = serde_json::to_string(&telemetry).unwrap();
          msg.push_str("\n");
          stream.write_all(msg.as_bytes())?;
        }
        Some(Commands::Receive {}) => {
          let mut stream = UnixStream::connect(SOCKET_PATH)?;
          println!("Connected");
          
          let mut buf = [0u8; 1024];
          let mut message_acc = String::new();
          loop {
            // Read response
            let n = stream.read(&mut buf)?;
            message_acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            while let Some(idx) = message_acc.find('\n') {
                println!("Received: {}", message_acc[..idx].to_string());
                message_acc = message_acc[idx+1..].to_string();
            }
          }
        }
        None => Cli::command().print_help().unwrap()
      }

    Ok(())
}