use anyhow::Result;
use async_trait::async_trait;
use futures::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use serde::{Deserialize, Serialize};
use std::{any::Any, collections::VecDeque, fmt::Display, sync::Arc};
use tokio::sync::Mutex;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

#[async_trait]
pub trait ReadHalf<R>: Send + Sync + 'static
where
    R: for<'de> Deserialize<'de> + Send + 'static,
{
    async fn read(&mut self) -> Result<R, std::io::Error>;
}

#[async_trait]
pub trait WriteHalf<T>: Send + Sync + 'static
where
    T: Serialize + Send + 'static,
{
    async fn write(&mut self, msg: T) -> Result<(), std::io::Error>;
}

#[async_trait]
pub trait Stream<R, T>: Send + Sync + 'static + Any
where
    R: for<'de> Deserialize<'de> + Send + 'static,
    T: Serialize + Send + 'static,
{
    async fn read(&mut self) -> Result<R, std::io::Error>;
    async fn write(&mut self, msg: T) -> Result<(), std::io::Error>;
    fn split(self: Box<Self>) -> (Box<dyn ReadHalf<R>>, Box<dyn WriteHalf<T>>);
}

#[async_trait]
pub trait TransportHandle<R, T>: Send + Sync + 'static
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static,
{
    async fn connect(&self) -> Result<Box<dyn Stream<T, R>>, std::io::Error>;
}

#[async_trait]
pub trait Transport<R, T>: Send + Sync + Display
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static,
{
    async fn accept(&mut self) -> Result<Box<dyn Stream<R, T>>, std::io::Error>;
    async fn connect(&self) -> Result<Box<dyn Stream<T, R>>, std::io::Error>;
    async fn channel(
        &mut self,
    ) -> Result<(Box<dyn Stream<T, R>>, Box<dyn Stream<R, T>>), std::io::Error> {
        let client_stream = self.connect().await?; // Initiate client connection
        let server_stream = self.accept().await?; // Accept client connection
        Ok((client_stream, server_stream))
    }
    fn handle(&self) -> Box<dyn TransportHandle<R, T>>;
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
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    async fn read(&mut self) -> Result<R, std::io::Error> {
        let bytes = self.inner.next().await.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Connection closed")
        })??;
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
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
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
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    async fn read(&mut self) -> Result<R, std::io::Error> {
        let bytes = self.framed_stream.next().await.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Connection closed")
        })??;
        bincode::deserialize(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    async fn write(&mut self, msg: T) -> Result<(), std::io::Error> {
        let bytes = bincode::serialize(&msg)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.framed_stream.send(bytes.into()).await
    }
    fn split(self: Box<Self>) -> (Box<dyn ReadHalf<R>>, Box<dyn WriteHalf<T>>) {
        let (write, read) = self.framed_stream.split();
        (
            Box::new(UnixReadHalf {
                inner: read,
                _r: std::marker::PhantomData,
            }),
            Box::new(UnixWriteHalf {
                inner: write,
                _t: std::marker::PhantomData,
            }),
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
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    async fn connect(&self) -> Result<Box<dyn Stream<T, R>>, std::io::Error> {
        match tokio::net::UnixStream::connect(self.path.clone()).await {
            Ok(stream) => Ok(Box::new(UnixStream {
                framed_stream: Framed::new(stream, LengthDelimitedCodec::new()),
                _r: std::marker::PhantomData,
                _t: std::marker::PhantomData,
            })),
            Err(e) => {
                eprintln!("Connection error: {}", e);
                Err(e)
            }
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
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Socket path must end with .sock",
            ));
        }
        // Remove socket if it already exists
        if std::path::Path::new(&path).exists() {
            tokio::fs::remove_file(&path).await?;
        }
        let listener = tokio::net::UnixListener::bind(path)?;
        Ok(Self {
            path: path.to_string(),
            listener,
            _r: std::marker::PhantomData,
            _t: std::marker::PhantomData,
        })
    }
}

#[async_trait]
impl<R, T> Transport<R, T> for UnixTransport<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    async fn accept(&mut self) -> Result<Box<dyn Stream<R, T>>, std::io::Error> {
        match self.listener.accept().await {
            Ok((stream, _)) => Ok(Box::new(UnixStream {
                framed_stream: Framed::new(stream, LengthDelimitedCodec::new()),
                _r: std::marker::PhantomData,
                _t: std::marker::PhantomData,
            })),
            Err(e) => {
                eprintln!("Connection error: {}", e);
                Err(e)
            }
        }
    }
    async fn connect(&self) -> Result<Box<dyn Stream<T, R>>, std::io::Error> {
        self.handle().connect().await
    }
    fn handle(&self) -> Box<dyn TransportHandle<R, T>> {
        Box::new(UnixTransportHandle {
            path: self.path.clone(),
            _r: std::marker::PhantomData,
            _t: std::marker::PhantomData,
        })
    }
}

impl<R, T> Display for UnixTransport<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Unix Socket {}", self.path.as_str())
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
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    async fn read(&mut self) -> Result<R, std::io::Error> {
        let bytes = self.inner.next().await.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Connection closed")
        })??;
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
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
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
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    async fn read(&mut self) -> Result<R, std::io::Error> {
        let bytes = self.framed_stream.next().await.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Connection closed")
        })??;
        bincode::deserialize(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
    async fn write(&mut self, msg: T) -> Result<(), std::io::Error> {
        let bytes = bincode::serialize(&msg)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.framed_stream.send(bytes.into()).await
    }
    fn split(self: Box<Self>) -> (Box<dyn ReadHalf<R>>, Box<dyn WriteHalf<T>>) {
        let (write, read) = self.framed_stream.split();
        (
            Box::new(TcpReadHalf {
                inner: read,
                _r: std::marker::PhantomData,
            }),
            Box::new(TcpWriteHalf {
                inner: write,
                _t: std::marker::PhantomData,
            }),
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
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    async fn connect(&self) -> Result<Box<dyn Stream<T, R>>, std::io::Error> {
        let full_address = format!("{}:{}", self.address, self.port);
        match tokio::net::TcpStream::connect(full_address.clone()).await {
            Ok(stream) => Ok(Box::new(TcpStream {
                framed_stream: Framed::new(stream, LengthDelimitedCodec::new()),
                _r: std::marker::PhantomData,
                _t: std::marker::PhantomData,
            })),
            Err(e) => {
                eprintln!("Connection error: {}", e);
                Err(e)
            }
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
    pub async fn new(address: &str, port: u16) -> Result<Self, std::io::Error> { // TODO: Rename to try_new for all transports which return Result
        let full_address = format!("{address}:{port}");
        let listener = tokio::net::TcpListener::bind(full_address.clone()).await?;
        let s = Self {
            address: address.to_string(),
            port,
            listener,
            _r: std::marker::PhantomData,
            _t: std::marker::PhantomData,
        };
        Ok(s)
    }
}

#[async_trait]
impl<R, T> Transport<R, T> for TcpTransport<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    async fn accept(&mut self) -> Result<Box<dyn Stream<R, T>>, std::io::Error> {
        match self.listener.accept().await {
            Ok((stream, _)) => Ok(Box::new(TcpStream {
                framed_stream: Framed::new(stream, LengthDelimitedCodec::new()),
                _r: std::marker::PhantomData,
                _t: std::marker::PhantomData,
            })),
            Err(e) => {
                eprintln!("Connection error: {}", e);
                Err(e)
            }
        }
    }
    async fn connect(&self) -> Result<Box<dyn Stream<T, R>>, std::io::Error> {
        self.handle().connect().await
    }
    fn handle(&self) -> Box<dyn TransportHandle<R, T>> {
        Box::new(TcpTransportHandle {
            address: self.address.clone(),
            port: self.port,
            _r: std::marker::PhantomData,
            _t: std::marker::PhantomData,
        })
    }
}

impl<R, T> Display for TcpTransport<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TCP Socket {}:{}", self.address, self.port)
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
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    async fn read(&mut self) -> Result<R, std::io::Error> {
        self.rx
            .recv()
            .await
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Channel closed"))
    }
}
pub struct MpscWriteHalf<T> {
    tx: tokio::sync::mpsc::Sender<T>,
}
#[async_trait]
impl<T> WriteHalf<T> for MpscWriteHalf<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    async fn write(&mut self, msg: T) -> Result<(), std::io::Error> {
        self.tx
            .send(msg)
            .await
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
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    async fn read(&mut self) -> Result<R, std::io::Error> {
        self.rx
            .recv()
            .await
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Channel closed"))
    }

    async fn write(&mut self, msg: T) -> Result<(), std::io::Error> {
        self.tx
            .send(msg)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Channel closed"))
    }
    fn split(self: Box<Self>) -> (Box<dyn ReadHalf<R>>, Box<dyn WriteHalf<T>>) {
        (Box::new(MpscReadHalf { rx: self.rx }), Box::new(MpscWriteHalf { tx: self.tx }))
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
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    async fn connect(&self) -> Result<Box<dyn Stream<T, R>>, std::io::Error> {
        let (tx_to_client, rx_in_client) = tokio::sync::mpsc::channel::<T>(self.buffer);
        let (tx_from_client, rx_from_client) = tokio::sync::mpsc::channel::<R>(self.buffer);
        let mut pending = self.pending.lock().await;
        pending.push_back((tx_to_client, rx_from_client));
        Ok(Box::new(MpscStream {
            rx: rx_in_client,
            tx: tx_from_client,
        }))
    }
}

pub struct MpscTransport<R, T> {
    buffer: usize,
    pending: Arc<Mutex<VecDeque<(tokio::sync::mpsc::Sender<T>, tokio::sync::mpsc::Receiver<R>)>>>,
}

impl<R, T> MpscTransport<R, T> {
    pub fn new(buffer: usize) -> Self {
        Self {
            buffer,
            pending: Arc::new(Mutex::new(VecDeque::new())),
        } // TODO: Init vecdeque with capacity?
    }
}

#[async_trait]
impl<R, T> Transport<R, T> for MpscTransport<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    async fn accept(&mut self) -> Result<Box<dyn Stream<R, T>>, std::io::Error> {
        let (tx_to_client, rx_from_client) = loop {
            let mut pending = self.pending.lock().await;
            if let Some(conn) = pending.pop_front() {
                break conn;
            }
            drop(pending);
            tokio::task::yield_now().await;
        };
        Ok(Box::new(MpscStream {
            rx: rx_from_client,
            tx: tx_to_client,
        }))
    }
    async fn connect(&self) -> Result<Box<dyn Stream<T, R>>, std::io::Error> {
        self.handle().connect().await
    }
    fn handle(&self) -> Box<dyn TransportHandle<R, T>> {
        Box::new(MpscTransportHandle {
            buffer: self.buffer,
            pending: self.pending.clone(),
        })
    }
}

impl<R, T> Display for MpscTransport<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MPSC Channel")
    }
}

// ============================================================================
// Test Transport (for unit testing only) - implemented on top of MpscTransport
// ============================================================================

pub struct TestReadHalf<R> {
    wrapped: Box<dyn ReadHalf<R>>,
    queue: Arc<Mutex<VecDeque<R>>>,
}
#[async_trait]
impl<R> ReadHalf<R> for TestReadHalf<R>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + Clone,
{
    async fn read(&mut self) -> Result<R, std::io::Error> {
        let value = self.wrapped.read().await;
        if let Ok(ref value) = value {
          let mut q = self.queue.lock().await;
          q.push_back(value.clone());
        }
        value
    }
}
pub struct TestWriteHalf<T> {
    wrapped: Box<dyn WriteHalf<T>>,
    queue: Arc<Mutex<VecDeque<T>>>,
}
#[async_trait]
impl<T> WriteHalf<T> for TestWriteHalf<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + Clone,
{
    async fn write(&mut self, msg: T) -> Result<(), std::io::Error> {
        {
            let mut q = self.queue.lock().await;
            q.push_back(msg.clone());
        }
        self.wrapped.write(msg).await
    }
}

pub struct TestStream<R, T> {
    wrapped: Box<dyn Stream<R, T>>,
    rx_queue: Arc<Mutex<VecDeque<R>>>,
    tx_queue: Arc<Mutex<VecDeque<T>>>,
}

#[async_trait]
impl<R, T> Stream<R, T> for TestStream<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + Clone,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + Clone,
{
    async fn read(&mut self) -> Result<R, std::io::Error> {
        let value = self.wrapped.read().await;
        if let Ok(ref value) = value {
          let mut q = self.rx_queue.lock().await;
          q.push_back(value.clone());
        }
        value
    }

    async fn write(&mut self, msg: T) -> Result<(), std::io::Error> {
        {
            let mut q = self.tx_queue.lock().await;
            q.push_back(msg.clone());
        }
        self.wrapped.write(msg).await
    }
    fn split(self: Box<Self>) -> (Box<dyn ReadHalf<R>>, Box<dyn WriteHalf<T>>) {
        let (rx_wrapped, tx_wrapped) = self.wrapped.split();
        (
          Box::new(TestReadHalf { wrapped: rx_wrapped, queue: self.rx_queue.clone() }), 
          Box::new(TestWriteHalf { wrapped: tx_wrapped, queue: self.tx_queue.clone() })
        )
    }
}

pub struct TestTransportHandle<R, T> {
    wrapped: Box<dyn TransportHandle<R, T>>,
}
#[async_trait]
impl<R, T> TransportHandle<R, T> for TestTransportHandle<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + Clone,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + Clone,
{
    async fn connect(&self) -> Result<Box<dyn Stream<T, R>>, std::io::Error> {
        self.wrapped.connect().await.map(|wrapped_stream| Box::new(TestStream {
            wrapped: wrapped_stream,
            rx_queue: Arc::new(Mutex::new(VecDeque::new())),
            tx_queue: Arc::new(Mutex::new(VecDeque::new())),
        }) as Box<dyn Stream<T, R>>)
    }
}

pub struct TestTransport<R, T> {
    wrapped: MpscTransport<R, T>, // TODO: Make generic to wrap any other transport type
    tx_queue: Arc<Mutex<VecDeque<T>>>,
    rx_queue: Arc<Mutex<VecDeque<R>>>,
}

impl<R, T> TestTransport<R, T>
where
    R: Clone,
    T: Clone,
{
    pub fn new(buffer: usize) -> Self {
        Self {
            wrapped: MpscTransport::new(buffer),
            tx_queue: Arc::new(Mutex::new(VecDeque::new())),
            rx_queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
}

#[async_trait]
impl<R, T> Transport<R, T> for TestTransport<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + Clone,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + Clone,
{
    async fn accept(&mut self) -> Result<Box<dyn Stream<R, T>>, std::io::Error> {
        self.wrapped.accept().await.map(|wrapped_stream| Box::new(TestStream {
            wrapped: wrapped_stream,
            rx_queue: self.rx_queue.clone(),
            tx_queue: self.tx_queue.clone(),
        }) as Box<dyn Stream<R, T>>)
    }
    async fn connect(&self) -> Result<Box<dyn Stream<T, R>>, std::io::Error> {
        self.wrapped.connect().await.map(|wrapped_stream| Box::new(TestStream {
            wrapped: wrapped_stream,
            rx_queue: Arc::new(Mutex::new(VecDeque::new())),
            tx_queue: Arc::new(Mutex::new(VecDeque::new())),
        }) as Box<dyn Stream<T, R>>)
    }
    fn handle(&self) -> Box<dyn TransportHandle<R, T>> {
        let wrapped = self.wrapped.handle();
        Box::new(TestTransportHandle {
            wrapped,
        })
    }
}

impl<R, T> Display for TestTransport<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TestTransport({})", self.wrapped)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use super::*;
    use serde::{Deserialize, Serialize};
    use tokio::time::timeout;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct TxMsg {
        value: u32,
    }
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct RxMsg {
        value: u32,
    }

    #[tokio::test]
    async fn test_unix_transport() {
        let path = "/tmp/safe_test.sock";
        let mut server = UnixTransport::<RxMsg, TxMsg>::new(path).await.unwrap();
        let handle = server.handle();
        let mut client_stream = handle.connect().await.unwrap();
        let mut server_stream = server.accept().await.unwrap();

        // Write from client, read from server
        client_stream.write(RxMsg { value: 42 }).await.unwrap();
        let msg = server_stream.read().await.unwrap();
        assert_eq!(msg, RxMsg { value: 42 });

        // Write from server, read from client
        server_stream.write(TxMsg { value: 99 }).await.unwrap();
        let msg = client_stream.read().await.unwrap();
        assert_eq!(msg, TxMsg { value: 99 });

        // Assert no broadcast by default
        let mut other_client_stream = handle.connect().await.unwrap();
        let mut other_server_stream = server.accept().await.unwrap();
        server_stream.write(TxMsg { value: 77 }).await.unwrap();
        let res = timeout(Duration::from_millis(100), other_client_stream.read()).await;
        assert!(res.is_err(), "Other client should not receive message");
        let msg = client_stream.read().await.unwrap();
        assert_eq!(msg, TxMsg { value: 77 });
        // Test initial channel still functional
        other_server_stream
            .write(TxMsg { value: 88 })
            .await
            .unwrap();
        let msg = other_client_stream.read().await.unwrap();
        assert_eq!(msg, TxMsg { value: 88 });
        let res = timeout(Duration::from_millis(100), client_stream.read()).await;
        assert!(res.is_err(), "Initial client should not receive message");

        // Test channel helper functionality
        let (mut client_stream, mut server_stream) = server.channel().await.unwrap();
        client_stream.write(RxMsg { value: 999 }).await.unwrap();
        let msg = server_stream.read().await.unwrap();
        assert_eq!(msg, RxMsg { value: 999 });
    }

    #[tokio::test]
    async fn test_tcp_transport() {
        let mut server = TcpTransport::<RxMsg, TxMsg>::new("127.0.0.1", 18080)
            .await
            .unwrap();
        let handle = server.handle();
        let mut client_stream = handle.connect().await.unwrap();
        let mut server_stream = server.accept().await.unwrap();

        // Write from client, read from server
        client_stream.write(RxMsg { value: 123 }).await.unwrap();
        let msg = server_stream.read().await.unwrap();
        assert_eq!(msg, RxMsg { value: 123 });

        // Write from server, read from client
        server_stream.write(TxMsg { value: 456 }).await.unwrap();
        let msg = client_stream.read().await.unwrap();
        assert_eq!(msg, TxMsg { value: 456 });

        // Assert no broadcast by default
        let mut other_client_stream = handle.connect().await.unwrap();
        let mut other_server_stream = server.accept().await.unwrap();
        server_stream.write(TxMsg { value: 77 }).await.unwrap();
        let res = timeout(Duration::from_millis(100), other_client_stream.read()).await;
        assert!(res.is_err(), "Other client should not receive message");
        let msg = client_stream.read().await.unwrap();
        assert_eq!(msg, TxMsg { value: 77 });
        // Test initial channel still functional
        other_server_stream
            .write(TxMsg { value: 88 })
            .await
            .unwrap();
        let msg = other_client_stream.read().await.unwrap();
        assert_eq!(msg, TxMsg { value: 88 });
        let res = timeout(Duration::from_millis(100), client_stream.read()).await;
        assert!(res.is_err(), "Initial client should not receive message");

        // Test channel helper functionality
        let (mut client_stream, mut server_stream) = server.channel().await.unwrap();
        client_stream.write(RxMsg { value: 999 }).await.unwrap();
        let msg = server_stream.read().await.unwrap();
        assert_eq!(msg, RxMsg { value: 999 });
    }

    #[tokio::test]
    async fn test_mpsc_transport() {
        let mut server = MpscTransport::<RxMsg, TxMsg>::new(8);
        let handle = server.handle();
        let mut client_stream = handle.connect().await.unwrap();
        let mut server_stream = server.accept().await.unwrap();

        // Write from client, read from server
        client_stream.write(RxMsg { value: 7 }).await.unwrap();
        let msg = server_stream.read().await.unwrap();
        assert_eq!(msg, RxMsg { value: 7 });

        // Write from server, read from client
        server_stream.write(TxMsg { value: 8 }).await.unwrap();
        let msg = client_stream.read().await.unwrap();
        assert_eq!(msg, TxMsg { value: 8 });

        // Assert no broadcast by default
        let mut other_client_stream = handle.connect().await.unwrap();
        let mut other_server_stream = server.accept().await.unwrap();
        server_stream.write(TxMsg { value: 77 }).await.unwrap();
        let res = timeout(Duration::from_millis(100), other_client_stream.read()).await;
        assert!(res.is_err(), "Other client should not receive message");
        let msg = client_stream.read().await.unwrap();
        assert_eq!(msg, TxMsg { value: 77 });
        // Test initial channel still functional
        other_server_stream
            .write(TxMsg { value: 88 })
            .await
            .unwrap();
        let msg = other_client_stream.read().await.unwrap();
        assert_eq!(msg, TxMsg { value: 88 });
        let res = timeout(Duration::from_millis(100), client_stream.read()).await;
        assert!(res.is_err(), "Initial client should not receive message");

        // Test channel helper functionality
        let (mut client_stream, mut server_stream) = server.channel().await.unwrap();
        client_stream.write(RxMsg { value: 999 }).await.unwrap();
        let msg = server_stream.read().await.unwrap();
        assert_eq!(msg, RxMsg { value: 999 });
    }

    async fn assert_ownership_model(mut transport: impl Transport<RxMsg, TxMsg> + 'static) {
        let handle = transport.handle();
        let lock = Arc::new(Mutex::new(())); // Only for synchronization in this test
        let lock_clone = lock.clone();

        // Move client stream to another task
        let client_task = tokio::spawn(async move {
            let _ = lock_clone.lock().await;
            let mut client_stream = handle.connect().await.unwrap();
            client_stream.write(RxMsg { value: 1 }).await.unwrap();
            let msg = client_stream.read().await.unwrap();
            assert_eq!(msg, TxMsg { value: 101 });
        });

        let other_handle = transport.handle();

        // Move server stream to another task
        let server_task = tokio::spawn(async move {
            let mut i = 0;
            while i < 3 {
                let mut server_stream = transport.accept().await.unwrap();
                let msg = server_stream.read().await.unwrap();
                assert_eq!(msg, RxMsg { value: i });
                server_stream.write(TxMsg { value: i + 100 }).await.unwrap();
                i += 1;
            }
        });

        // Confirm driver process can still communicate with server
        {
            let mut other_client_stream = other_handle.connect().await.unwrap();
            let _ = lock.lock().await;
            other_client_stream.write(RxMsg { value: 0 }).await.unwrap();
            let msg = other_client_stream.read().await.unwrap();
            assert_eq!(msg, TxMsg { value: 100 });
        }

        // Test splitting streams
        let (mut read, mut write) = other_handle.connect().await.unwrap().split();
        let write_task = tokio::spawn(async move {
            write.write(RxMsg { value: 2 }).await.unwrap();
        });
        let read_task = tokio::spawn(async move {
            let msg = read.read().await.unwrap();
            assert_eq!(msg, TxMsg { value: 102 });
        });

        client_task.await.unwrap();
        server_task.await.unwrap();
        write_task.await.unwrap();
        read_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_mpsc_ownership_model() {
        let transport = MpscTransport::<RxMsg, TxMsg>::new(8);
        assert_ownership_model(transport).await;
    }

    #[tokio::test]
    async fn test_unix_ownership_model() {
        let transport = UnixTransport::<RxMsg, TxMsg>::new("/tmp/safe_ownership_test.sock")
            .await
            .unwrap();
        assert_ownership_model(transport).await;
    }

    #[tokio::test]
    async fn test_tcp_ownership_model() {
        let transport = TcpTransport::<RxMsg, TxMsg>::new("127.0.0.1", 10000)
            .await
            .unwrap();
        assert_ownership_model(transport).await;
    }

    async fn assert_perf(mut transport: impl Transport<RxMsg, TxMsg> + 'static) -> f64 {
        let handle = transport.handle();
        let mut client_stream = handle.connect().await.unwrap();
        let mut server_stream = transport.accept().await.unwrap();

        let iterations = 10000;
        let start = tokio::time::Instant::now();
        for i in 0..iterations {
            client_stream.write(RxMsg { value: i }).await.unwrap();
            let msg = server_stream.read().await.unwrap();
            assert_eq!(msg, RxMsg { value: i });
        }
        let duration = start.elapsed();
        let avg_latency = duration.as_micros() as f64 / iterations as f64;
        println!(
            "Transport average round-trip latency over {} iterations: {:.2} µs",
            iterations, avg_latency
        );
        avg_latency
    }

    #[tokio::test]
    async fn test_mpsc_perf() {
        let transport = MpscTransport::<RxMsg, TxMsg>::new(1024);
        assert!(assert_perf(transport).await < 2.0); // Expect under 2 µs RTT
    }

    #[tokio::test]
    async fn test_unix_perf() {
        let transport = UnixTransport::<RxMsg, TxMsg>::new("/tmp/safe_perf_test.sock")
            .await
            .unwrap();
        assert!(assert_perf(transport).await < 20.0); // Expect under 20 µs RTT
    }

    #[tokio::test]
    async fn test_tcp_perf() {
        let transport = TcpTransport::<RxMsg, TxMsg>::new("127.0.0.1", 10001)
            .await
            .unwrap();
        assert!(assert_perf(transport).await < 60.0); // Expect under 60 µs RTT
    }

    #[tokio::test]
    async fn test_test_transport() {
        let mut server = TestTransport::<RxMsg, TxMsg>::new(1024);
        let handle = server.handle();
        let server_rx_queue = server.rx_queue.clone();
        let server_tx_queue = server.tx_queue.clone();
        let mut client_stream = handle.connect().await.unwrap();
        let downcasted_client_stream = (&*client_stream as &dyn Any).downcast_ref::<TestStream<TxMsg, RxMsg>>().unwrap();
        let client_rx_queue = downcasted_client_stream.rx_queue.clone();
        let client_tx_queue = downcasted_client_stream.tx_queue.clone();
        let mut server_stream = server.accept().await.unwrap();

        client_stream.write(RxMsg { value: 7 }).await.unwrap();
        server_stream.write(TxMsg { value: 70 }).await.unwrap();
        client_stream.write(RxMsg { value: 8 }).await.unwrap();
        server_stream.write(TxMsg { value: 80 }).await.unwrap();
        assert_eq!(server_tx_queue.lock().await.iter().map(|r| r.clone()).collect::<Vec<TxMsg>>(), vec![TxMsg { value: 70 }, TxMsg { value: 80 }]);
        assert_eq!(client_tx_queue.lock().await.iter().map(|r| r.clone()).collect::<Vec<RxMsg>>(), vec![RxMsg { value: 7 }, RxMsg { value: 8 }]);
        assert_eq!(server_rx_queue.lock().await.iter().map(|r| r.clone()).collect::<Vec<RxMsg>>(), vec![]); // queue empty until message read, which is good
        assert_eq!(client_rx_queue.lock().await.iter().map(|r| r.clone()).collect::<Vec<TxMsg>>(), vec![]); // queue empty until message read, which is good
        
        // Test server reads
        server_stream.read().await.unwrap();
        assert_eq!(server_rx_queue.lock().await.iter().map(|r| r.clone()).collect::<Vec<RxMsg>>(), vec![RxMsg { value: 7 }]);
        server_stream.read().await.unwrap();
        assert_eq!(server_rx_queue.lock().await.iter().map(|r| r.clone()).collect::<Vec<RxMsg>>(), vec![RxMsg { value: 7 }, RxMsg { value: 8 }]);
        
        // Test client reads
        client_stream.read().await.unwrap();
        assert_eq!(client_rx_queue.lock().await.iter().map(|r| r.clone()).collect::<Vec<TxMsg>>(), vec![TxMsg { value: 70 }]);
        client_stream.read().await.unwrap();
        assert_eq!(client_rx_queue.lock().await.iter().map(|r| r.clone()).collect::<Vec<TxMsg>>(), vec![TxMsg { value: 70 }, TxMsg { value: 80 }]);
        
        // Assert final state of queues is as expected
        assert_eq!(client_tx_queue.lock().await.iter().map(|r| r.clone()).collect::<Vec<RxMsg>>(), vec![RxMsg { value: 7 }, RxMsg { value: 8 }]);
        assert_eq!(client_rx_queue.lock().await.iter().map(|r| r.clone()).collect::<Vec<TxMsg>>(), vec![TxMsg { value: 70 }, TxMsg { value: 80 }]);
        assert_eq!(server_tx_queue.lock().await.iter().map(|r| r.clone()).collect::<Vec<TxMsg>>(), vec![TxMsg { value: 70 }, TxMsg { value: 80 }]);
        assert_eq!(server_rx_queue.lock().await.iter().map(|r| r.clone()).collect::<Vec<RxMsg>>(), vec![RxMsg { value: 7 }, RxMsg { value: 8 }]);
    }
}

// TODO:
// - Implement timeouts on reads?
// - Implement connection retries and reconnect on any transport that can be disrupted?
