use std::os::unix::net::UnixStream;
use std::io::Write;

use serde::{Deserialize, Serialize};
use std::env;

const SOCKET_PATH: &str = "/tmp/safe.sock";

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Telemetry {
    pub timestamp: u64,
    pub proximity_m: i32,
}


fn main() -> std::io::Result<()> {
    let mut stream = UnixStream::connect(SOCKET_PATH)?;
    println!("Connected to server");
    
    // Send message to server
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
      eprintln!("Usage: {} <json_string>", args[0]);
      std::process::exit(1);
    }
    
    let telemetry: Telemetry = serde_json::from_str(&args[1])
      .expect("Failed to parse JSON string");
    let mut msg = serde_json::to_string(&telemetry).unwrap();
    msg.push_str("\n");
    stream.write_all(msg.as_bytes())?;
    
    // Read response
    // let mut buf = [0u8; 1024];
    // let n = stream.read(&mut buf)?;
    // let response: Telemetry = serde_json::from_str(&String::from_utf8_lossy(&buf[..n])).unwrap();
    // println!("Response: {:?}", response);
    
    Ok(())
}