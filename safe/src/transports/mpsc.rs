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

use crate::transports::traits::{ReadHalf, Stream, Transport, TransportHandle, WriteHalf};

#[derive(Debug)]
pub struct MpscReadHalf<R> {
    rx: tokio::sync::mpsc::Receiver<R>,
}

#[async_trait]
impl<R> ReadHalf<R> for MpscReadHalf<R>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
{
    async fn read(&mut self) -> Result<R, std::io::Error> {
        self.rx
            .recv()
            .await
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Channel closed"))
    }
}

#[derive(Debug)]
pub struct MpscWriteHalf<T> {
    tx: tokio::sync::mpsc::Sender<T>,
}
#[async_trait]
impl<T> WriteHalf<T> for MpscWriteHalf<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
{
    async fn write(&mut self, msg: T) -> Result<(), std::io::Error> {
        self.tx
            .send(msg)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Channel closed"))
    }
}

#[derive(Debug)]
pub struct MpscStream<R, T> {
    rx: tokio::sync::mpsc::Receiver<R>,
    tx: tokio::sync::mpsc::Sender<T>,
}

#[async_trait]
impl<R, T> Stream<R, T> for MpscStream<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
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
        (
            Box::new(MpscReadHalf { rx: self.rx }),
            Box::new(MpscWriteHalf { tx: self.tx }),
        )
    }
}

pub type PendingTransportHandles<R, T> =
    VecDeque<(tokio::sync::mpsc::Sender<T>, tokio::sync::mpsc::Receiver<R>)>;

#[derive(Debug)]
pub struct MpscTransportHandle<R, T> {
    buffer: usize,
    pending: Arc<Mutex<PendingTransportHandles<R, T>>>,
}
#[async_trait]
impl<R, T> TransportHandle<R, T> for MpscTransportHandle<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
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

#[derive(Debug)]
pub struct MpscTransport<R, T> {
    buffer: usize,
    pending: Arc<Mutex<PendingTransportHandles<R, T>>>,
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
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
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
