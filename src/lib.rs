//! MOQ-Iroh: A standalone protocol for real-time media streaming over Iroh.
//!
//! This module implements a P2P streaming protocol inspired by Media over QUIC (MoQ),
//! leveraging Iroh's gossip for signaling and a custom QUIC protocol for control and data
//! transfer. It supports full MoQ parity, including publish/subscribe, named objects,
//! control/data streams, QoS, retransmission, and relay functionality.

// Re-export submodules and key types for convenience
pub mod moq;

pub use moq::client::MoqIrohClient;
pub use moq::proto::{ MoqObject, MoqRelay, StreamAnnouncement, ANNOUNCE_TOPIC, ALPN };
pub use moq::protocol::MoqIroh;

// Ensure all necessary traits and types are available
pub use iroh::protocol::ProtocolHandler;
