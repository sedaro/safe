use std::{any::Any, collections::VecDeque, fmt::Display, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::transports::{
    mpsc, tcp,
    traits::{ReadHalf, Stream, Transport, TransportHandle, WriteHalf},
    unix,
};

#[derive(Debug)]
pub struct TestReadHalf<R> {
    wrapped: Box<dyn ReadHalf<R>>,
    queue: Arc<Mutex<VecDeque<R>>>,
}
#[async_trait]
impl<R> ReadHalf<R> for TestReadHalf<R>
where
    R: Serialize
        + for<'de> Deserialize<'de>
        + Send
        + 'static
        + std::marker::Sync
        + Clone
        + std::fmt::Debug,
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

#[derive(Debug)]
pub struct TestWriteHalf<T> {
    wrapped: Box<dyn WriteHalf<T>>,
    queue: Arc<Mutex<VecDeque<T>>>,
}
#[async_trait]
impl<T> WriteHalf<T> for TestWriteHalf<T>
where
    T: Serialize
        + for<'de> Deserialize<'de>
        + Send
        + 'static
        + std::marker::Sync
        + Clone
        + std::fmt::Debug,
{
    async fn write(&mut self, msg: T) -> Result<(), std::io::Error> {
        {
            let mut q = self.queue.lock().await;
            q.push_back(msg.clone());
        }
        self.wrapped.write(msg).await
    }
}

#[derive(Debug)]
pub struct TestStream<R, T> {
    wrapped: Box<dyn Stream<R, T>>,
    rx_queue: Arc<Mutex<VecDeque<R>>>,
    tx_queue: Arc<Mutex<VecDeque<T>>>,
}

#[async_trait]
impl<R, T> Stream<R, T> for TestStream<R, T>
where
    R: Serialize
        + for<'de> Deserialize<'de>
        + Send
        + 'static
        + std::marker::Sync
        + Clone
        + std::fmt::Debug,
    T: Serialize
        + for<'de> Deserialize<'de>
        + Send
        + 'static
        + std::marker::Sync
        + Clone
        + std::fmt::Debug,
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
            Box::new(TestReadHalf {
                wrapped: rx_wrapped,
                queue: self.rx_queue.clone(),
            }),
            Box::new(TestWriteHalf {
                wrapped: tx_wrapped,
                queue: self.tx_queue.clone(),
            }),
        )
    }
}

#[derive(Debug)]
pub struct TestTransportHandle<R, T> {
    wrapped: Box<dyn TransportHandle<R, T>>,
}
#[async_trait]
impl<R, T> TransportHandle<R, T> for TestTransportHandle<R, T>
where
    R: Serialize
        + for<'de> Deserialize<'de>
        + Send
        + 'static
        + std::marker::Sync
        + Clone
        + std::fmt::Debug,
    T: Serialize
        + for<'de> Deserialize<'de>
        + Send
        + 'static
        + std::marker::Sync
        + Clone
        + std::fmt::Debug,
{
    async fn connect(&self) -> Result<Box<dyn Stream<T, R>>, std::io::Error> {
        self.wrapped.connect().await.map(|wrapped_stream| {
            Box::new(TestStream {
                wrapped: wrapped_stream,
                rx_queue: Arc::new(Mutex::new(VecDeque::new())),
                tx_queue: Arc::new(Mutex::new(VecDeque::new())),
            }) as Box<dyn Stream<T, R>>
        })
    }
}

#[derive(Debug)]
pub struct TestTransport<R, T> {
    wrapped: mpsc::MpscTransport<R, T>, // TODO: Make generic to wrap any other transport type
    tx_queue: Arc<Mutex<VecDeque<T>>>,
    rx_queue: Arc<Mutex<VecDeque<R>>>,
}

impl<R, T> TestTransport<R, T>
where
    R: Clone + std::fmt::Debug,
    T: Clone + std::fmt::Debug,
{
    pub fn new(buffer: usize) -> Self {
        Self {
            wrapped: mpsc::MpscTransport::new(buffer),
            tx_queue: Arc::new(Mutex::new(VecDeque::new())),
            rx_queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
}

#[async_trait]
impl<R, T> Transport<R, T> for TestTransport<R, T>
where
    R: Serialize
        + for<'de> Deserialize<'de>
        + Send
        + 'static
        + std::marker::Sync
        + Clone
        + std::fmt::Debug,
    T: Serialize
        + for<'de> Deserialize<'de>
        + Send
        + 'static
        + std::marker::Sync
        + Clone
        + std::fmt::Debug,
{
    async fn accept(&mut self) -> Result<Box<dyn Stream<R, T>>, std::io::Error> {
        self.wrapped.accept().await.map(|wrapped_stream| {
            Box::new(TestStream {
                wrapped: wrapped_stream,
                rx_queue: self.rx_queue.clone(),
                tx_queue: self.tx_queue.clone(),
            }) as Box<dyn Stream<R, T>>
        })
    }
    async fn connect(&self) -> Result<Box<dyn Stream<T, R>>, std::io::Error> {
        self.wrapped.connect().await.map(|wrapped_stream| {
            Box::new(TestStream {
                wrapped: wrapped_stream,
                rx_queue: Arc::new(Mutex::new(VecDeque::new())),
                tx_queue: Arc::new(Mutex::new(VecDeque::new())),
            }) as Box<dyn Stream<T, R>>
        })
    }
    fn handle(&self) -> Box<dyn TransportHandle<R, T>> {
        let wrapped = self.wrapped.handle();
        Box::new(TestTransportHandle { wrapped })
    }
}

impl<R, T> Display for TestTransport<R, T>
where
    R: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static + std::marker::Sync + std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TestTransport({})", self.wrapped)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde::{Deserialize, Serialize};
    use tokio::time::timeout;

    use super::*;

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
        let mut server = unix::UnixTransport::<RxMsg, TxMsg>::new(path)
            .await
            .unwrap();
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
        let mut server = tcp::TcpTransport::<RxMsg, TxMsg>::new("127.0.0.1", 18080)
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
        let mut server = mpsc::MpscTransport::<RxMsg, TxMsg>::new(8);
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
        let transport = mpsc::MpscTransport::<RxMsg, TxMsg>::new(8);
        assert_ownership_model(transport).await;
    }

    #[tokio::test]
    async fn test_unix_ownership_model() {
        let transport = unix::UnixTransport::<RxMsg, TxMsg>::new("/tmp/safe_ownership_test.sock")
            .await
            .unwrap();
        assert_ownership_model(transport).await;
    }

    #[tokio::test]
    async fn test_tcp_ownership_model() {
        let transport = tcp::TcpTransport::<RxMsg, TxMsg>::new("127.0.0.1", 10000)
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
        let transport = mpsc::MpscTransport::<RxMsg, TxMsg>::new(1024);
        assert!(assert_perf(transport).await < 2.0); // Expect under 2 µs RTT
    }

    #[tokio::test]
    async fn test_unix_perf() {
        let transport = unix::UnixTransport::<RxMsg, TxMsg>::new("/tmp/safe_perf_test.sock")
            .await
            .unwrap();
        assert!(assert_perf(transport).await < 20.0); // Expect under 20 µs RTT
    }

    #[tokio::test]
    async fn test_tcp_perf() {
        let transport = tcp::TcpTransport::<RxMsg, TxMsg>::new("127.0.0.1", 10001)
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
        let downcasted_client_stream = (&*client_stream as &dyn Any)
            .downcast_ref::<TestStream<TxMsg, RxMsg>>()
            .unwrap();
        let client_rx_queue = downcasted_client_stream.rx_queue.clone();
        let client_tx_queue = downcasted_client_stream.tx_queue.clone();
        let mut server_stream = server.accept().await.unwrap();

        client_stream.write(RxMsg { value: 7 }).await.unwrap();
        server_stream.write(TxMsg { value: 70 }).await.unwrap();
        client_stream.write(RxMsg { value: 8 }).await.unwrap();
        server_stream.write(TxMsg { value: 80 }).await.unwrap();
        assert_eq!(
            server_tx_queue
                .lock()
                .await
                .iter()
                .map(|r| r.clone())
                .collect::<Vec<TxMsg>>(),
            vec![TxMsg { value: 70 }, TxMsg { value: 80 }]
        );
        assert_eq!(
            client_tx_queue
                .lock()
                .await
                .iter()
                .map(|r| r.clone())
                .collect::<Vec<RxMsg>>(),
            vec![RxMsg { value: 7 }, RxMsg { value: 8 }]
        );
        assert_eq!(
            server_rx_queue
                .lock()
                .await
                .iter()
                .map(|r| r.clone())
                .collect::<Vec<RxMsg>>(),
            vec![]
        ); // queue empty until message read, which is good
        assert_eq!(
            client_rx_queue
                .lock()
                .await
                .iter()
                .map(|r| r.clone())
                .collect::<Vec<TxMsg>>(),
            vec![]
        ); // queue empty until message read, which is good

        // Test server reads
        server_stream.read().await.unwrap();
        assert_eq!(
            server_rx_queue
                .lock()
                .await
                .iter()
                .map(|r| r.clone())
                .collect::<Vec<RxMsg>>(),
            vec![RxMsg { value: 7 }]
        );
        server_stream.read().await.unwrap();
        assert_eq!(
            server_rx_queue
                .lock()
                .await
                .iter()
                .map(|r| r.clone())
                .collect::<Vec<RxMsg>>(),
            vec![RxMsg { value: 7 }, RxMsg { value: 8 }]
        );

        // Test client reads
        client_stream.read().await.unwrap();
        assert_eq!(
            client_rx_queue
                .lock()
                .await
                .iter()
                .map(|r| r.clone())
                .collect::<Vec<TxMsg>>(),
            vec![TxMsg { value: 70 }]
        );
        client_stream.read().await.unwrap();
        assert_eq!(
            client_rx_queue
                .lock()
                .await
                .iter()
                .map(|r| r.clone())
                .collect::<Vec<TxMsg>>(),
            vec![TxMsg { value: 70 }, TxMsg { value: 80 }]
        );

        // Assert final state of queues is as expected
        assert_eq!(
            client_tx_queue
                .lock()
                .await
                .iter()
                .map(|r| r.clone())
                .collect::<Vec<RxMsg>>(),
            vec![RxMsg { value: 7 }, RxMsg { value: 8 }]
        );
        assert_eq!(
            client_rx_queue
                .lock()
                .await
                .iter()
                .map(|r| r.clone())
                .collect::<Vec<TxMsg>>(),
            vec![TxMsg { value: 70 }, TxMsg { value: 80 }]
        );
        assert_eq!(
            server_tx_queue
                .lock()
                .await
                .iter()
                .map(|r| r.clone())
                .collect::<Vec<TxMsg>>(),
            vec![TxMsg { value: 70 }, TxMsg { value: 80 }]
        );
        assert_eq!(
            server_rx_queue
                .lock()
                .await
                .iter()
                .map(|r| r.clone())
                .collect::<Vec<RxMsg>>(),
            vec![RxMsg { value: 7 }, RxMsg { value: 8 }]
        );
    }
}
