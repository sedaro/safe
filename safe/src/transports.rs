use anyhow::Result;
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::c2::{Command, Telemetry};
use crate::config::ConfigMessage;

// TODO: Revisit

// ============================================================================
// C2 Transport Abstraction
// ============================================================================

#[async_trait]
pub trait C2Transport: Send + Sync {
    async fn recv_telemetry(&mut self) -> Result<Telemetry>;
    async fn send_command(&mut self, cmd: Command) -> Result<()>;
}

struct TcpC2Transport {
    framed: Framed<TcpStream, LengthDelimitedCodec>,
}

impl TcpC2Transport {
    fn new(stream: TcpStream) -> Self {
        Self {
            framed: Framed::new(stream, LengthDelimitedCodec::new()),
        }
    }
}

#[async_trait]
impl C2Transport for TcpC2Transport {
    async fn recv_telemetry(&mut self) -> Result<Telemetry> {
        let bytes = self
            .framed
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("Connection closed"))??;
        Ok(bincode::deserialize(&bytes)?)
    }

    async fn send_command(&mut self, cmd: Command) -> Result<()> {
        let bytes = bincode::serialize(&cmd)?;
        self.framed.send(bytes.into()).await?;
        Ok(())
    }
}

// ============================================================================
// Config Transport
// ============================================================================

#[async_trait]
pub trait ConfigTransport: Send + Sync {
    async fn recv_config(&mut self) -> Result<ConfigMessage>;
    async fn send_response(&mut self, response: String) -> Result<()>;
}

struct TcpConfigTransport {
    framed: Framed<TcpStream, LengthDelimitedCodec>,
}

impl TcpConfigTransport {
    fn new(stream: TcpStream) -> Self {
        Self {
            framed: Framed::new(stream, LengthDelimitedCodec::new()),
        }
    }
}

#[async_trait]
impl ConfigTransport for TcpConfigTransport {
    async fn recv_config(&mut self) -> Result<ConfigMessage> {
        let bytes = self
            .framed
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("Connection closed"))??;
        Ok(bincode::deserialize(&bytes)?)
    }

    async fn send_response(&mut self, response: String) -> Result<()> {
        let bytes = response.into_bytes();
        self.framed.send(bytes.into()).await?;
        Ok(())
    }
}