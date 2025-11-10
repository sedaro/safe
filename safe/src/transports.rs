use anyhow::Result;
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

// TODO: Revisit

#[async_trait]
pub trait Stream: Send + Sync {
  async fn read(&mut self) -> Result<String, std::io::Error>; // TODO: Interface via bytes instead?
  async fn write(&mut self, msg: String) -> Result<(), std::io::Error>;
}
#[async_trait]
pub trait Transport: Send + Sync {
    async fn accept(&mut self) -> Result<impl Stream, std::io::Error>;
}

// ============================================================================
// Unix Socket
// ============================================================================

pub struct UnixStream {
  framed_stream: Framed<tokio::net::UnixStream, LengthDelimitedCodec>,
}
#[async_trait]
impl Stream for UnixStream {
  async fn read(&mut self) -> Result<String, std::io::Error> {
    return self.framed_stream.next().await
      .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Connection closed"))?
      .map(|bytes| String::from_utf8_lossy(&bytes).to_string());
  }
  async fn write(&mut self, msg: String) -> Result<(), std::io::Error> {
    return self.framed_stream.send(msg.into()).await;
  }
}
pub struct UnixTransport {
    path: String,
    listener: tokio::net::UnixListener,
}
impl UnixTransport {
  pub async fn new(path: String) -> Result<Self, std::io::Error> {
      let listener = tokio::net::UnixListener::bind(path.clone())?;
      println!("SAFE listening on {}", path);
      Ok(Self { path, listener })
  }
}
#[async_trait]
impl Transport for UnixTransport {
  async fn accept(&mut self) -> Result<UnixStream, std::io::Error> {
    match self.listener.accept().await {
      Ok((stream, _)) => Ok(UnixStream { 
        framed_stream: Framed::new(stream, LengthDelimitedCodec::new()),
      }),
      Err(e) => {
        eprintln!("Connection error: {}", e);
        Err(e)
      },
    }
  }
}

pub struct TcpStream {
  framed_stream: Framed<tokio::net::TcpStream, LengthDelimitedCodec>,
}
#[async_trait]
impl Stream for TcpStream {
  async fn read(&mut self) -> Result<String, std::io::Error> {
    return self.framed_stream.next().await
      .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Connection closed"))?
      .map(|bytes| String::from_utf8_lossy(&bytes).to_string());
  }
  async fn write(&mut self, msg: String) -> Result<(), std::io::Error> {
    return self.framed_stream.send(msg.into()).await;
  }
}
pub struct TcpTransport {
    address: String,
    port: u16,
    listener: tokio::net::TcpListener,
}
impl TcpTransport {
  async fn new(address: String, port: u16) -> Result<Self, std::io::Error> {
    let full_address = format!("{address}:{port}");
    let listener = tokio::net::TcpListener::bind(full_address.clone()).await?;
    println!("SAFE listening on {}", full_address);
    let s = Self { address, port, listener };
    Ok(s)
  }
}
#[async_trait]
impl Transport for TcpTransport {
  async fn accept(&mut self) -> Result<TcpStream, std::io::Error> {
    match self.listener.accept().await {
      Ok((stream, _)) => Ok(TcpStream { 
        framed_stream: Framed::new(stream, LengthDelimitedCodec::new()),
      }),
      Err(e) => {
        eprintln!("Connection error: {}", e);
        Err(e)
      },
    }
  }
}

// ============================================================================
// C2 Transport Abstraction
// ============================================================================

// #[async_trait]
// pub trait C2Transport: Send + Sync {
//     async fn recv_telemetry(&mut self) -> Result<Telemetry>;
//     async fn send_command(&mut self, cmd: Command) -> Result<()>;
// }

// struct TcpC2Transport {
//     framed: Framed<TcpStream, LengthDelimitedCodec>,
// }

// impl TcpC2Transport {
//     fn new(stream: TcpStream) -> Self {
//         Self {
//             framed: Framed::new(stream, LengthDelimitedCodec::new()),
//         }
//     }
// }

// #[async_trait]
// impl C2Transport for TcpC2Transport {
//     async fn recv_telemetry(&mut self) -> Result<Telemetry> {
//         let bytes = self
//             .framed
//             .next()
//             .await
//             .ok_or_else(|| anyhow::anyhow!("Connection closed"))??;
//         Ok(bincode::deserialize(&bytes)?)
//     }

//     async fn send_command(&mut self, cmd: Command) -> Result<()> {
//         let bytes = bincode::serialize(&cmd)?;
//         self.framed.send(bytes.into()).await?;
//         Ok(())
//     }
// }

// ============================================================================
// Config Transport
// ============================================================================

// #[async_trait]
// pub trait ConfigTransport: Send + Sync {
//     async fn recv_config(&mut self) -> Result<ConfigMessage>;
//     async fn send_response(&mut self, response: String) -> Result<()>;
// }

// struct TcpConfigTransport {
//     framed: Framed<TcpStream, LengthDelimitedCodec>,
// }

// impl TcpConfigTransport {
//     fn new(stream: TcpStream) -> Self {
//         Self {
//             framed: Framed::new(stream, LengthDelimitedCodec::new()),
//         }
//     }
// }

// #[async_trait]
// impl ConfigTransport for TcpConfigTransport {
//     async fn recv_config(&mut self) -> Result<ConfigMessage> {
//         let bytes = self
//             .framed
//             .next()
//             .await
//             .ok_or_else(|| anyhow::anyhow!("Connection closed"))??;
//         Ok(bincode::deserialize(&bytes)?)
//     }

//     async fn send_response(&mut self, response: String) -> Result<()> {
//         let bytes = response.into_bytes();
//         self.framed.send(bytes.into()).await?;
//         Ok(())
//     }
// }