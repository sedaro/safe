use anyhow::Result;
use async_trait::async_trait;
use futures::{SinkExt, StreamExt, stream::{SplitSink, SplitStream}};
use tokio::sync::Mutex;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use serde::{Serialize, Deserialize};
use std::{collections::VecDeque, sync::Arc};

#[async_trait]
pub trait Stream<T>: Send + Sync + 'static
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
    type StreamType: Stream<T> + Send;
    async fn accept(&mut self) -> Result<Self::StreamType, std::io::Error>;
    async fn connect(&self) -> Result<Self::StreamType, std::io::Error>;
    async fn channel(&mut self) -> Result<(Self::StreamType, Self::StreamType), std::io::Error> {
        let client_stream = self.connect().await?; // Initiate client connection
        let server_stream = self.accept().await?; // Accept client connection
        Ok((client_stream, server_stream))
    }
}

// ============================================================================
// Unix Socket
// ============================================================================

pub struct UnixStream<T> {
  framed_stream: Framed<tokio::net::UnixStream, LengthDelimitedCodec>,
  _phantom: std::marker::PhantomData<T>,
}

#[async_trait]
impl<T> Stream<T> for UnixStream<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
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
      // Require path ends in .sock to protect against accidental file deletion
      if !path.ends_with(".sock") {
          return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "Socket path must end with .sock"));
      }
      // Remove socket if it already exists
      if std::path::Path::new(&path).exists() {
          tokio::fs::remove_file(&path).await?;
      }
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
  type StreamType = UnixStream<T>;
  async fn accept(&mut self) -> Result<Self::StreamType, std::io::Error> {
    match self.listener.accept().await {
      Ok((stream, _)) => Ok(UnixStream { 
        framed_stream: Framed::new(stream, LengthDelimitedCodec::new()),
        _phantom: std::marker::PhantomData,
      }),
      Err(e) => {
        eprintln!("Connection error: {}", e);
        Err(e)
      },
    }
  }
  async fn connect(&self) -> Result<Self::StreamType, std::io::Error> {
    match tokio::net::UnixStream::connect(self.path.clone()).await {
      Ok(stream) => Ok(UnixStream {
        framed_stream: Framed::new(stream, LengthDelimitedCodec::new()),
        _phantom: std::marker::PhantomData,
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

pub struct TcpStream<T> {
  framed_stream: Framed<tokio::net::TcpStream, LengthDelimitedCodec>,
  _phantom: std::marker::PhantomData<T>,
}

#[async_trait]
impl<T> Stream<T> for TcpStream<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
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

pub struct TcpTransport<T> {
    address: String,
    port: u16,
    listener: tokio::net::TcpListener,
    _phantom: std::marker::PhantomData<T>,
}

impl<T> TcpTransport<T> {
  async fn new(address: String, port: u16) -> Result<Self, std::io::Error> {
    let full_address = format!("{address}:{port}");
    let listener = tokio::net::TcpListener::bind(full_address.clone()).await?;
    println!("SAFE listening on {}", full_address);
    let s = Self { address, port, listener, _phantom: std::marker::PhantomData };
    Ok(s)
  }
}

#[async_trait]
impl<T> Transport<T> for TcpTransport<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
  type StreamType = TcpStream<T>;
  async fn accept(&mut self) -> Result<Self::StreamType, std::io::Error> {
    match self.listener.accept().await {
      Ok((stream, _)) => {
        Ok(TcpStream { 
        framed_stream: Framed::new(stream, LengthDelimitedCodec::new()),
        _phantom: std::marker::PhantomData,
        })
      },
      Err(e) => {
        eprintln!("Connection error: {}", e);
        Err(e)
      },
    }
  }
  async fn connect(&self) -> Result<Self::StreamType, std::io::Error> {
    let full_address = format!("{}:{}", self.address, self.port);
    match tokio::net::TcpStream::connect(full_address.clone()).await {
      Ok(stream) => Ok(TcpStream {
        framed_stream: Framed::new(stream, LengthDelimitedCodec::new()),
        _phantom: std::marker::PhantomData,
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
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + Clone
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
    pending: Arc<Mutex<VecDeque<(tokio::sync::mpsc::Sender<T>, tokio::sync::mpsc::Receiver<T>)>>>,
}

impl<T: Clone> MpscTransport<T> {
    pub fn new(buffer: usize) -> Self {
        Self { buffer, pending: Arc::new(Mutex::new(VecDeque::new())) } // TODO: Init vecdeque with capacity?
    }
}

#[async_trait]
impl<T> Transport<T> for MpscTransport<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + Clone
{
    type StreamType = MpscStream<T>;
    async fn accept(&mut self) -> Result<Self::StreamType, std::io::Error> {
        let (tx_to_client, rx_from_client) = loop {
            let mut pending = self.pending.lock().await;
            if let Some(conn) = pending.pop_front() {
              break conn;
            }
            drop(pending);
            tokio::task::yield_now().await;
        };
        Ok(MpscStream { rx: rx_from_client, tx: tx_to_client })
    }
    async fn connect(&self) -> Result<Self::StreamType, std::io::Error> {
        let (tx_to_client, rx_in_client) = tokio::sync::mpsc::channel::<T>(self.buffer);
        let (tx_from_client, rx_from_client) = tokio::sync::mpsc::channel::<T>(self.buffer);
        let mut pending = self.pending.lock().await;
        pending.push_back((tx_to_client, rx_from_client));
        Ok(MpscStream { rx: rx_in_client, tx: tx_from_client })
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