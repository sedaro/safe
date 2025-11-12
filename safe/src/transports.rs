use anyhow::Result;
use async_trait::async_trait;
use futures::{SinkExt, StreamExt, stream::{SplitSink, SplitStream}};
use tokio::sync::Mutex;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use serde::{Serialize, Deserialize};
use std::{collections::VecDeque, sync::Arc};

#[async_trait]
pub trait ReadHalf<R>: Send + Sync + 'static
where
    R: for<'de> Deserialize<'de> + Send + 'static
{
  async fn read(&mut self) -> Result<R, std::io::Error>;
}

#[async_trait]
pub trait WriteHalf<T>: Send + Sync + 'static
where
    T: Serialize + Send + 'static
{
  async fn write(&mut self, msg: T) -> Result<(), std::io::Error>;
}

#[async_trait]
pub trait Stream<R, T>: Send + Sync + 'static
where
    R: for<'de> Deserialize<'de> + Send + 'static,
    T: Serialize + Send + 'static
{
  async fn read(&mut self) -> Result<R, std::io::Error>;
  async fn write(&mut self, msg: T) -> Result<(), std::io::Error>;
  fn split(self) -> (impl ReadHalf<R>, impl WriteHalf<T>);
}

#[async_trait]
pub trait TransportHandle<R, T>: Send + Sync + 'static
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static
{
  type ClientStreamType: Stream<T, R> + Send;
  async fn connect(&self) -> Result<Self::ClientStreamType, std::io::Error>;
}

#[async_trait]
pub trait Transport<R, T>: Send + Sync
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static
{
    type ServerStreamType: Stream<R, T> + Send;
    type ClientStreamType: Stream<T, R> + Send;
    type TransportHandleType: TransportHandle<R, T> + Send;
    async fn accept(&mut self) -> Result<Self::ServerStreamType, std::io::Error>;
    async fn connect(&self) -> Result<Self::ClientStreamType, std::io::Error>;
    async fn channel(&mut self) -> Result<(Self::ClientStreamType, Self::ServerStreamType), std::io::Error> {
        let client_stream = self.connect().await?; // Initiate client connection
        let server_stream = self.accept().await?; // Accept client connection
        Ok((client_stream, server_stream))
    }
    fn handle(&self) -> Self::TransportHandleType;
}

// ============================================================================
// Unix Socket
// ============================================================================

pub struct UnixReadHalf<R> {
  inner: SplitStream<Framed<tokio::net::UnixStream, LengthDelimitedCodec>>,
  _r: std::marker::PhantomData<R>,
}
#[async_trait]
impl<R> ReadHalf<R> for UnixReadHalf<R>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
  async fn read(&mut self) -> Result<R, std::io::Error> {
    let bytes = self.inner.next().await
      .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Connection closed"))??;
    bincode::deserialize(&bytes)
      .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
  }
}

pub struct UnixWriteHalf<T> {
  inner: SplitSink<Framed<tokio::net::UnixStream, LengthDelimitedCodec>, bytes::Bytes>,
  _t: std::marker::PhantomData<T>,
}
#[async_trait]
impl<T> WriteHalf<T> for UnixWriteHalf<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
  async fn write(&mut self, msg: T) -> Result<(), std::io::Error> {
    let bytes = bincode::serialize(&msg)
      .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    self.inner.send(bytes.into()).await
  }
}
pub struct UnixStream<R, T> {
  framed_stream: Framed<tokio::net::UnixStream, LengthDelimitedCodec>,
  _r: std::marker::PhantomData<R>,
  _t: std::marker::PhantomData<T>,
}

#[async_trait]
impl<R, T> Stream<R, T> for UnixStream<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
  async fn read(&mut self) -> Result<R, std::io::Error> {
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
  fn split(self) -> (impl ReadHalf<R>, impl WriteHalf<T>) {
    let (write, read) = self.framed_stream.split();
    (
      UnixReadHalf { inner: read, _r: std::marker::PhantomData },
      UnixWriteHalf { inner: write, _t: std::marker::PhantomData },
    )
  }
}

pub struct UnixTransportHandle<R, T> {
  path: String,
  _r: std::marker::PhantomData<R>,
  _t: std::marker::PhantomData<T>,
}

#[async_trait]
impl<R, T> TransportHandle<R, T> for UnixTransportHandle<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
  type ClientStreamType = UnixStream<T, R>;
  async fn connect(&self) -> Result<Self::ClientStreamType, std::io::Error> {
    match tokio::net::UnixStream::connect(self.path.clone()).await {
      Ok(stream) => Ok(UnixStream {
        framed_stream: Framed::new(stream, LengthDelimitedCodec::new()),
        _r: std::marker::PhantomData,
        _t: std::marker::PhantomData,
      }),
      Err(e) => {
        eprintln!("Connection error: {}", e);
        Err(e)
      },
    }
  }
}

pub struct UnixTransport<R, T> {
    path: String,
    listener: tokio::net::UnixListener,
    _r: std::marker::PhantomData<R>,
    _t: std::marker::PhantomData<T>,
}

impl<R, T> UnixTransport<R, T> {
  pub async fn new(path: &str) -> Result<Self, std::io::Error> {
      // Require path ends in .sock to protect against accidental file deletion
      if !path.ends_with(".sock") {
          return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "Socket path must end with .sock"));
      }
      // Remove socket if it already exists
      if std::path::Path::new(&path).exists() {
          tokio::fs::remove_file(&path).await?;
      }
      let listener = tokio::net::UnixListener::bind(path)?;
      println!("SAFE listening on {}", path);
      Ok(Self { path: path.to_string(), listener, _r: std::marker::PhantomData, _t: std::marker::PhantomData })
  }
}

#[async_trait]
impl<R, T> Transport<R, T> for UnixTransport<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
  type ServerStreamType = UnixStream<R, T>;
  type ClientStreamType = UnixStream<T, R>;
  type TransportHandleType = UnixTransportHandle<R, T>;
  async fn accept(&mut self) -> Result<Self::ServerStreamType, std::io::Error> {
    match self.listener.accept().await {
      Ok((stream, _)) => Ok(UnixStream { 
        framed_stream: Framed::new(stream, LengthDelimitedCodec::new()),
        _r: std::marker::PhantomData,
        _t: std::marker::PhantomData,
      }),
      Err(e) => {
        eprintln!("Connection error: {}", e);
        Err(e)
      },
    }
  }
  async fn connect(&self) -> Result<Self::ClientStreamType, std::io::Error> {
    self.handle().connect().await
  }
  fn handle(&self) -> Self::TransportHandleType {
    UnixTransportHandle {
      path: self.path.clone(),
      _r: std::marker::PhantomData,
      _t: std::marker::PhantomData,
    }
  }
}

// ============================================================================
// TCP Socket
// ============================================================================

pub struct TcpReadHalf<R> {
  inner: SplitStream<Framed<tokio::net::TcpStream, LengthDelimitedCodec>>,
  _r: std::marker::PhantomData<R>,
}
#[async_trait]
impl<R> ReadHalf<R> for TcpReadHalf<R>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
  async fn read(&mut self) -> Result<R, std::io::Error> {
    let bytes = self.inner.next().await
      .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Connection closed"))??;
    bincode::deserialize(&bytes)
      .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
  }
}

pub struct TcpWriteHalf<T> {
  inner: SplitSink<Framed<tokio::net::TcpStream, LengthDelimitedCodec>, bytes::Bytes>,
  _t: std::marker::PhantomData<T>,
}
#[async_trait]
impl<T> WriteHalf<T> for TcpWriteHalf<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
  async fn write(&mut self, msg: T) -> Result<(), std::io::Error> {
    let bytes = bincode::serialize(&msg)
      .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    self.inner.send(bytes.into()).await
  }
}

pub struct TcpStream<R, T> {
  framed_stream: Framed<tokio::net::TcpStream, LengthDelimitedCodec>,
  _r: std::marker::PhantomData<R>,
  _t: std::marker::PhantomData<T>,
}

#[async_trait]
impl<R, T> Stream<R, T> for TcpStream<R, T>
where
    R: Serialize + for<'de> Deserialize<'de>+ Send + 'static + std::marker::Sync,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
  async fn read(&mut self) -> Result<R, std::io::Error> {
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
  fn split(self) -> (impl ReadHalf<R>, impl WriteHalf<T>) {
    let (write, read) = self.framed_stream.split();
    (
      TcpReadHalf { inner: read, _r: std::marker::PhantomData },
      TcpWriteHalf { inner: write, _t: std::marker::PhantomData },
    )
  }
}

pub struct TcpTransportHandle<R, T> {
    address: String,
    port: u16,
    _r: std::marker::PhantomData<R>,
    _t: std::marker::PhantomData<T>,
}
#[async_trait]
impl<R, T> TransportHandle<R, T> for TcpTransportHandle<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
  type ClientStreamType = TcpStream<T, R>;
  async fn connect(&self) -> Result<Self::ClientStreamType, std::io::Error> {
    let full_address = format!("{}:{}", self.address, self.port);
    match tokio::net::TcpStream::connect(full_address.clone()).await {
      Ok(stream) => Ok(TcpStream {  
        framed_stream: Framed::new(stream, LengthDelimitedCodec::new()),
        _r: std::marker::PhantomData,
        _t: std::marker::PhantomData,
      }),
      Err(e) => {
        eprintln!("Connection error: {}", e);
        Err(e)
      },
    }
  }
}

pub struct TcpTransport<R, T> {
    address: String,
    port: u16,
    listener: tokio::net::TcpListener,
    _r: std::marker::PhantomData<R>,
    _t: std::marker::PhantomData<T>,
}

impl<R, T> TcpTransport<R, T> {
  pub async fn new(address: &str, port: u16) -> Result<Self, std::io::Error> {
    let full_address = format!("{address}:{port}");
    let listener = tokio::net::TcpListener::bind(full_address.clone()).await?;
    println!("SAFE listening on {}", full_address);
    let s = Self { address: address.to_string(), port, listener, _r: std::marker::PhantomData, _t: std::marker::PhantomData };
    Ok(s)
  }
}

#[async_trait]
impl<R, T> Transport<R, T> for TcpTransport<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
  type ServerStreamType = TcpStream<R, T>;
  type ClientStreamType = TcpStream<T, R>;
  type TransportHandleType = TcpTransportHandle<R, T>;
  async fn accept(&mut self) -> Result<Self::ServerStreamType, std::io::Error> {
    match self.listener.accept().await {
      Ok((stream, _)) => {
        Ok(TcpStream { 
        framed_stream: Framed::new(stream, LengthDelimitedCodec::new()),
        _r: std::marker::PhantomData,
        _t: std::marker::PhantomData,
        })
      },
      Err(e) => {
        eprintln!("Connection error: {}", e);
        Err(e)
      },
    }
  }
  async fn connect(&self) -> Result<Self::ClientStreamType, std::io::Error> {
    self.handle().connect().await
  }
  fn handle(&self) -> Self::TransportHandleType {
    TcpTransportHandle {
      address: self.address.clone(),
      port: self.port,
      _r: std::marker::PhantomData,
      _t: std::marker::PhantomData,
    }
  }
}

// ============================================================================
// MPSC
// ============================================================================

pub struct MpscReadHalf<R> {
    rx: tokio::sync::mpsc::Receiver<R>,
}
#[async_trait]
impl<R> ReadHalf<R> for MpscReadHalf<R>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
    async fn read(&mut self) -> Result<R, std::io::Error> {
        self.rx.recv().await
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Channel closed"))
    }
}
pub struct MpscWriteHalf<T> {
    tx: tokio::sync::mpsc::Sender<T>,
}
#[async_trait]
impl<T> WriteHalf<T> for MpscWriteHalf<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
    async fn write(&mut self, msg: T) -> Result<(), std::io::Error> {
        self.tx.send(msg).await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Channel closed"))
    }
}

pub struct MpscStream<R, T> {
    rx: tokio::sync::mpsc::Receiver<R>,
    tx: tokio::sync::mpsc::Sender<T>,
}

#[async_trait]
impl<R, T> Stream<R, T> for MpscStream<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
    async fn read(&mut self) -> Result<R, std::io::Error> {
        self.rx.recv().await
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Channel closed"))
    }
    
    async fn write(&mut self, msg: T) -> Result<(), std::io::Error> {
        self.tx.send(msg).await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Channel closed"))
    }
    fn split(self) -> (impl ReadHalf<R>, impl WriteHalf<T>) {
        (
            MpscReadHalf { rx: self.rx },
            MpscWriteHalf { tx: self.tx },
        )
    }
}

pub struct MpscTransportHandle<R, T> {
    buffer: usize,
    pending: Arc<Mutex<VecDeque<(tokio::sync::mpsc::Sender<T>, tokio::sync::mpsc::Receiver<R>)>>>,
}
#[async_trait]
impl<R, T> TransportHandle<R, T> for MpscTransportHandle<R, T> 
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
    type ClientStreamType = MpscStream<T, R>;
    async fn connect(&self) -> Result<Self::ClientStreamType, std::io::Error> {
        let (tx_to_client, rx_in_client) = tokio::sync::mpsc::channel::<T>(self.buffer);
        let (tx_from_client, rx_from_client) = tokio::sync::mpsc::channel::<R>(self.buffer);
        let mut pending = self.pending.lock().await;
        pending.push_back((tx_to_client, rx_from_client));
        Ok(MpscStream { rx: rx_in_client, tx: tx_from_client })
    }
}

pub struct MpscTransport<R, T> {
    buffer: usize,
    pending: Arc<Mutex<VecDeque<(tokio::sync::mpsc::Sender<T>, tokio::sync::mpsc::Receiver<R>)>>>,
}

impl<R, T> MpscTransport<R, T> {
    pub fn new(buffer: usize) -> Self {
        Self { buffer, pending: Arc::new(Mutex::new(VecDeque::new())) } // TODO: Init vecdeque with capacity?
    }
}

#[async_trait]
impl<R, T> Transport<R, T> for MpscTransport<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync
{
    type ServerStreamType = MpscStream<R, T>;
    type ClientStreamType = MpscStream<T, R>;
    type TransportHandleType = MpscTransportHandle<R, T>;
    async fn accept(&mut self) -> Result<Self::ServerStreamType, std::io::Error> {
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
    async fn connect(&self) -> Result<Self::ClientStreamType, std::io::Error> {
        self.handle().connect().await
    }
    fn handle(&self) -> Self::TransportHandleType {
        MpscTransportHandle {
            buffer: self.buffer,
            pending: self.pending.clone(),
        }
    }
}

mod tests {
  use super::*;
  use serde::{Serialize, Deserialize};
  use tokio::time::{timeout, Duration};

  #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
  struct TestMsg {
      value: u32,
  }

  #[tokio::test]
  async fn test_unix_transport() {
      let path = "/tmp/safe_test.sock";
      let mut server = UnixTransport::<TestMsg, TestMsg>::new(path).await.unwrap();
      let handle = server.handle();
      let mut client_stream = handle.connect().await.unwrap();
      let mut server_stream = server.accept().await.unwrap();
      
      // Write from client, read from server
      client_stream.write(TestMsg { value: 42 }).await.unwrap();
      let msg = server_stream.read().await.unwrap();
      assert_eq!(msg, TestMsg { value: 42 });
      
      // Write from server, read from client
      server_stream.write(TestMsg { value: 99 }).await.unwrap();
      let msg = client_stream.read().await.unwrap();
      assert_eq!(msg, TestMsg { value: 99 });
      
      // Assert no broadcast by default
      let mut other_client_stream = handle.connect().await.unwrap();
      let mut other_server_stream= server.accept().await.unwrap();
      server_stream.write(TestMsg { value: 77 }).await.unwrap();
      let res = timeout(Duration::from_millis(100), other_client_stream.read()).await;
      assert!(res.is_err(), "Other client should not receive message");
      let msg = client_stream.read().await.unwrap();
      assert_eq!(msg, TestMsg { value: 77 });
      // Test initial channel still functional
      other_server_stream.write(TestMsg { value: 88 }).await.unwrap();
      let msg = other_client_stream.read().await.unwrap();
      assert_eq!(msg, TestMsg { value: 88 });
      let res = timeout(Duration::from_millis(100), client_stream.read()).await;
      assert!(res.is_err(), "Initial client should not receive message");

      // Test channel helper functionality
      let (mut client_stream, mut server_stream) = server.channel().await.unwrap();
      client_stream.write(TestMsg { value: 999 }).await.unwrap();
      let msg = server_stream.read().await.unwrap();
      assert_eq!(msg, TestMsg { value: 999 });
  }

  #[tokio::test]
  async fn test_tcp_transport() {
      let mut server = TcpTransport::<TestMsg, TestMsg>::new("127.0.0.1", 18080).await.unwrap();
      let handle = server.handle();
      let mut client_stream = handle.connect().await.unwrap();
      let mut server_stream = server.accept().await.unwrap();
      
      // Write from client, read from server
      client_stream.write(TestMsg { value: 123 }).await.unwrap();
      let msg = server_stream.read().await.unwrap();
      assert_eq!(msg, TestMsg { value: 123 });
      
      // Write from server, read from client
      server_stream.write(TestMsg { value: 456 }).await.unwrap();
      let msg = client_stream.read().await.unwrap();
      assert_eq!(msg, TestMsg { value: 456 });
      
      // Assert no broadcast by default
      let mut other_client_stream = handle.connect().await.unwrap();
      let mut other_server_stream= server.accept().await.unwrap();
      server_stream.write(TestMsg { value: 77 }).await.unwrap();
      let res = timeout(Duration::from_millis(100), other_client_stream.read()).await;
      assert!(res.is_err(), "Other client should not receive message");
      let msg = client_stream.read().await.unwrap();
      assert_eq!(msg, TestMsg { value: 77 });
      // Test initial channel still functional
      other_server_stream.write(TestMsg { value: 88 }).await.unwrap();
      let msg = other_client_stream.read().await.unwrap();
      assert_eq!(msg, TestMsg { value: 88 });
      let res = timeout(Duration::from_millis(100), client_stream.read()).await;
      assert!(res.is_err(), "Initial client should not receive message");

      // Test channel helper functionality
      let (mut client_stream, mut server_stream) = server.channel().await.unwrap();
      client_stream.write(TestMsg { value: 999 }).await.unwrap();
      let msg = server_stream.read().await.unwrap();
      assert_eq!(msg, TestMsg { value: 999 });
  }

  #[tokio::test]
  async fn test_mpsc_transport() {
      let mut server = MpscTransport::<TestMsg, TestMsg>::new(8);
      let handle = server.handle();
      let mut client_stream = handle.connect().await.unwrap();
      let mut server_stream = server.accept().await.unwrap();
      
      // Write from client, read from server
      client_stream.write(TestMsg { value: 7 }).await.unwrap();
      let msg = server_stream.read().await.unwrap();
      assert_eq!(msg, TestMsg { value: 7 });
      
      // Write from server, read from client
      server_stream.write(TestMsg { value: 8 }).await.unwrap();
      let msg = client_stream.read().await.unwrap();
      assert_eq!(msg, TestMsg { value: 8 });
      
      // Assert no broadcast by default
      let mut other_client_stream = handle.connect().await.unwrap();
      let mut other_server_stream= server.accept().await.unwrap();
      server_stream.write(TestMsg { value: 77 }).await.unwrap();
      let res = timeout(Duration::from_millis(100), other_client_stream.read()).await;
      assert!(res.is_err(), "Other client should not receive message");
      let msg = client_stream.read().await.unwrap();
      assert_eq!(msg, TestMsg { value: 77 });
      // Test initial channel still functional
      other_server_stream.write(TestMsg { value: 88 }).await.unwrap();
      let msg = other_client_stream.read().await.unwrap();
      assert_eq!(msg, TestMsg { value: 88 });
      let res = timeout(Duration::from_millis(100), client_stream.read()).await;
      assert!(res.is_err(), "Initial client should not receive message");

      // Test channel helper functionality
      let (mut client_stream, mut server_stream) = server.channel().await.unwrap();
      client_stream.write(TestMsg { value: 999 }).await.unwrap();
      let msg = server_stream.read().await.unwrap();
      assert_eq!(msg, TestMsg { value: 999 });
  }
}

// TODO:
// - Implement timeouts on reads?
// - Implement connection retries and reconnect on any transport that can be disrupted