#![allow(unused)]

pub mod mpsc;
pub mod tcp;
pub mod tests;
pub mod traits;
pub mod unix;

pub use crate::transports::{
    mpsc::MpscTransport,
    tcp::TcpTransport,
    tests::TestTransport,
    traits::{ReadHalf, Stream, Transport, TransportHandle, WriteHalf},
    unix::UnixTransport,
};

// TODO:
// - Implement timeouts on reads?
// - Implement connection retries and reconnect on any transport that can be disrupted?
