//! MOQ-Iroh: A standalone protocol for real-time media streaming over Iroh.
//!
//! This module implements a P2P streaming protocol inspired by Media over QUIC (MoQ),
//! leveraging Iroh's gossip for signaling and a custom QUIC protocol for control and data
//! transfer. It supports full MoQ parity, including publish/subscribe, named objects,
//! control/data streams, QoS, retransmission, and relay functionality.

// Re-export submodules and key types for convenience
pub mod client;
pub mod engine;
pub mod proto;
pub mod protocol;
pub mod video;
pub mod subscriber;

pub use client::MoqIrohClient;
pub use engine::MoqIrohEngine;
pub use protocol::{ MoqIroh, MoqIrohConfig };
pub use video::{ VideoSource, VideoFrame, VideoConfig, VideoStreaming };
pub use subscriber::subscribe_to_video_stream;

// Ensure all necessary traits and types are available
pub use iroh::protocol::ProtocolHandler;
