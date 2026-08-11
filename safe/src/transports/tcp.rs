use std::{any::Any, collections::VecDeque, fmt::Display, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::transports::traits::{ReadHalf, Stream, Transport, TransportHandle, WriteHalf};

#[derive(Debug)]
pub struct TcpReadHalf<R> {
    inner: SplitStream<Framed<tokio::net::TcpStream, LengthDelimitedCodec>>,
    _r: std::marker::PhantomData<R>,
}
#[async_trait]
impl<R> ReadHalf<R> for TcpReadHalf<R>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
{
    async fn read(&mut self) -> Result<R, std::io::Error> {
        let bytes = self.inner.next().await.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Connection closed")
        })??;
        bincode::deserialize(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

#[derive(Debug)]
pub struct TcpWriteHalf<T> {
    inner: SplitSink<Framed<tokio::net::TcpStream, LengthDelimitedCodec>, bytes::Bytes>,
    _t: std::marker::PhantomData<T>,
}
#[async_trait]
impl<T> WriteHalf<T> for TcpWriteHalf<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
{
    async fn write(&mut self, msg: T) -> Result<(), std::io::Error> {
        let bytes = bincode::serialize(&msg)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.inner.send(bytes.into()).await
    }
}

#[derive(Debug)]
pub struct TcpStream<R, T> {
    framed_stream: Framed<tokio::net::TcpStream, LengthDelimitedCodec>,
    _r: std::marker::PhantomData<R>,
    _t: std::marker::PhantomData<T>,
}

#[async_trait]
impl<R, T> Stream<R, T> for TcpStream<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
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

#[derive(Debug)]
pub struct TcpTransportHandle<R, T> {
    address: String,
    port: u16,
    _r: std::marker::PhantomData<R>,
    _t: std::marker::PhantomData<T>,
}
#[async_trait]
impl<R, T> TransportHandle<R, T> for TcpTransportHandle<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
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

#[derive(Debug)]
pub struct TcpTransport<R, T> {
    address: String,
    port: u16,
    listener: tokio::net::TcpListener,
    _r: std::marker::PhantomData<R>,
    _t: std::marker::PhantomData<T>,
}

impl<R, T> TcpTransport<R, T> {
    pub async fn new(address: &str, port: u16) -> Result<Self, std::io::Error> {
        // TODO: Rename to try_new for all transports which return Result
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
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
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
