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
pub struct UnixReadHalf<R> {
    inner: SplitStream<Framed<tokio::net::UnixStream, LengthDelimitedCodec>>,
    _r: std::marker::PhantomData<R>,
}
#[async_trait]
impl<R> ReadHalf<R> for UnixReadHalf<R>
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
pub struct UnixWriteHalf<T> {
    inner: SplitSink<Framed<tokio::net::UnixStream, LengthDelimitedCodec>, bytes::Bytes>,
    _t: std::marker::PhantomData<T>,
}
#[async_trait]
impl<T> WriteHalf<T> for UnixWriteHalf<T>
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
pub struct UnixStream<R, T> {
    framed_stream: Framed<tokio::net::UnixStream, LengthDelimitedCodec>,
    _r: std::marker::PhantomData<R>,
    _t: std::marker::PhantomData<T>,
}

#[async_trait]
impl<R, T> Stream<R, T> for UnixStream<R, T>
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

#[derive(Debug)]
pub struct UnixTransportHandle<R, T> {
    path: String,
    _r: std::marker::PhantomData<R>,
    _t: std::marker::PhantomData<T>,
}

impl<R, T> UnixTransportHandle<R, T> {
    pub fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
            _r: std::marker::PhantomData,
            _t: std::marker::PhantomData,
        }
    }
}

#[async_trait]
impl<R, T> TransportHandle<R, T> for UnixTransportHandle<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
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

#[derive(Debug)]
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
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
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
