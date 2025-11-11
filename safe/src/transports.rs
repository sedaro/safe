use anyhow::Result;
use async_trait::async_trait;
use futures::{SinkExt, StreamExt, stream::{SplitSink, SplitStream}};
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use serde::{Serialize, Deserialize};

#[async_trait]
pub trait Stream<T>: Send + Sync
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static
{
  async fn read(&mut self) -> Result<T, std::io::Error>;
  async fn write(&mut self, msg: T) -> Result<(), std::io::Error>;
}

#[async_trait]
pub trait Transport<T>: Send + Sync 
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static
{
    async fn accept(&mut self) -> Result<impl Stream<T>, std::io::Error>;
    async fn connect(&mut self) -> Result<impl Stream<T>, std::io::Error>;
}

// ============================================================================
// Unix Socket
// ============================================================================

pub struct UnixStream {
  framed_stream: Framed<tokio::net::UnixStream, LengthDelimitedCodec>,
}

#[async_trait]
impl<T> Stream<T> for UnixStream
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static
{
  async fn read(&mut self) -> Result<T, std::io::Error> {
    let bytes = self.framed_stream.next().await
      .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Connection closed"))??;
    bincode::deserialize(&bytes)
      .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
  }
  
  async fn write(&mut self, msg: T) -> Result<(), std::io::Error> {
    let bytes = bincode::serialize(&msg)
      .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    self.framed_stream.send(bytes.into()).await
  }
}

pub struct UnixTransport<T> {
    path: String,
    listener: tokio::net::UnixListener,
    _phantom: std::marker::PhantomData<T>,
}

impl<T> UnixTransport<T> {
  pub async fn new(path: String) -> Result<Self, std::io::Error> {
      let listener = tokio::net::UnixListener::bind(path.clone())?;
      println!("SAFE listening on {}", path);
      Ok(Self { path, listener, _phantom: std::marker::PhantomData })
  }
}

#[async_trait]
impl<T> Transport<T> for UnixTransport<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
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
  async fn connect(&mut self) -> Result<UnixStream, std::io::Error> {
    match tokio::net::UnixStream::connect(self.path.clone()).await {
      Ok(stream) => Ok(UnixStream {
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
// TCP Socket
// ============================================================================

pub struct TcpStream {
  framed_stream: Framed<tokio::net::TcpStream, LengthDelimitedCodec>,
}

#[async_trait]
impl<T> Stream<T> for TcpStream
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static
{
  async fn read(&mut self) -> Result<T, std::io::Error> {
    let bytes = self.framed_stream.next().await
      .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Connection closed"))??;
    bincode::deserialize(&bytes)
      .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
  }
  
  async fn write(&mut self, msg: T) -> Result<(), std::io::Error> {
    let bytes = bincode::serialize(&msg)
      .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    self.framed_stream.send(bytes.into()).await
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
impl<T> Transport<T> for TcpTransport
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static
{
  async fn accept(&mut self) -> Result<TcpStream, std::io::Error> {
    match self.listener.accept().await {
      Ok((stream, _)) => {
        Ok(TcpStream { 
        framed_stream: Framed::new(stream, LengthDelimitedCodec::new()),
        })
      },
      Err(e) => {
        eprintln!("Connection error: {}", e);
        Err(e)
      },
    }
  }
  async fn connect(&mut self) -> Result<TcpStream, std::io::Error> {
    let full_address = format!("{}:{}", self.address, self.port);
    match tokio::net::TcpStream::connect(full_address.clone()).await {
      Ok(stream) => Ok(TcpStream {
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
// MPSC
// ============================================================================

pub struct MpscStream<T> {
    rx: tokio::sync::mpsc::Receiver<T>,
    tx: tokio::sync::mpsc::Sender<T>,
}

#[async_trait]
impl<T> Stream<T> for MpscStream<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static
{
    async fn read(&mut self) -> Result<T, std::io::Error> {
        self.rx.recv().await
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Channel closed"))
    }
    
    async fn write(&mut self, msg: T) -> Result<(), std::io::Error> {
        self.tx.send(msg).await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Channel closed"))
    }
}

pub struct MpscTransport<T> {
    buffer: usize,
    rx_from_client: Option<tokio::sync::mpsc::Receiver<T>>,
    tx_to_client: tokio::sync::mpsc::Sender<T>,
    rx_in_client: Option<tokio::sync::mpsc::Receiver<T>>,
    tx_from_client: tokio::sync::mpsc::Sender<T>,
}

impl<T> MpscTransport<T> {
    pub fn new(buffer: usize) -> Self {
        let (tx_to_client, rx_in_client) = tokio::sync::mpsc::channel::<T>(buffer);
        let (tx_from_client, rx_from_client) = tokio::sync::mpsc::channel::<T>(buffer);
        Self { buffer, rx_from_client: Some(rx_from_client), tx_to_client, rx_in_client: Some(rx_in_client), tx_from_client }
    }
}

#[async_trait]
impl<T> Transport<T> for MpscTransport<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
    async fn accept(&mut self) -> Result<MpscStream<T>, std::io::Error> {
        let rx = self.rx_from_client.take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::AlreadyExists, "Accept already called. Implement broadcast channels if needed."))?;
        Ok(MpscStream { rx, tx: self.tx_to_client.clone() })
    }
    async fn connect(&mut self) -> Result<MpscStream<T>, std::io::Error> {
        let rx = self.rx_in_client.take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::AlreadyExists, "Connect already called. Implement broadcast channels if needed."))?;
        Ok(MpscStream { rx, tx: self.tx_from_client.clone() })
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