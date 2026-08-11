#![allow(unused)]

pub mod mpsc;
pub mod tcp;
#[cfg(test)]
pub mod tests;
pub mod traits;
pub mod unix;

#[cfg(test)]
pub use crate::transports::tests::TestTransport;
pub use crate::transports::{
    mpsc::MpscTransport,
    tcp::TcpTransport,
    traits::{ReadHalf, Stream, Transport, TransportHandle, WriteHalf},
    unix::UnixTransport,
};

// TODO:
// - Implement timeouts on reads?
// - Implement connection retries and reconnect on any transport that can be disrupted?
