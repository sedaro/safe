use std::{any::Any, collections::VecDeque, fmt::Display, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use futures::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

#[async_trait]
pub trait ReadHalf<R>: Send + Sync + 'static + std::fmt::Debug
where
    R: for<'de> Deserialize<'de> + Send + 'static,
{
    async fn read(&mut self) -> Result<R, std::io::Error>;
}

#[async_trait]
pub trait WriteHalf<T>: Send + Sync + 'static + std::fmt::Debug
where
    T: Serialize + Send + 'static,
{
    async fn write(&mut self, msg: T) -> Result<(), std::io::Error>;
}

#[async_trait]
pub trait Stream<R, T>: Send + Sync + 'static + Any + std::fmt::Debug
where
    R: for<'de> Deserialize<'de> + Send + 'static,
    T: Serialize + Send + 'static,
{
    async fn read(&mut self) -> Result<R, std::io::Error>;
    async fn write(&mut self, msg: T) -> Result<(), std::io::Error>;
    fn split(self: Box<Self>) -> (Box<dyn ReadHalf<R>>, Box<dyn WriteHalf<T>>);
}

#[async_trait]
pub trait TransportHandle<R, T>: Send + Sync + 'static + std::fmt::Debug
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::fmt::Debug,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::fmt::Debug,
{
    async fn connect(&self) -> Result<Box<dyn Stream<T, R>>, std::io::Error>;
}

#[async_trait]
pub trait Transport<R, T>: Send + Sync + Display + std::fmt::Debug
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::fmt::Debug,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::fmt::Debug,
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
