use super::proto::{
    StreamAnnouncement,
    MediaInit,
    VideoChunk,
    deserialize_announce,
    ANNOUNCE_TOPIC,
};
use super::protocol::MoqIroh;
use crate::moq::engine::ConnectionState;
use anyhow::Result;
use futures::StreamExt;
use iroh::Endpoint;
use iroh::NodeId;
use iroh_gossip::net::{ Gossip, Event, GossipEvent };
use tokio::sync::{ mpsc, broadcast };
use std::sync::Arc;
use tracing::{ info, debug, error };
use uuid::Uuid;
use blake3;
use tokio_stream::wrappers::ReceiverStream;
use std::time::Duration;
use anyhow::bail;
use crate::moq::subscriber;
use crate::moq::proto::{ AudioInit, AudioChunk };

/// A client for the iroh-moq protocol, providing a user-friendly interface
/// that delegates stream operations to the engine via the protocol handler.
#[derive(Clone)]
pub struct MoqIrohClient {
    protocol: MoqIroh,
    endpoint: Arc<Endpoint>,
    gossip: Arc<Gossip>,
}

impl MoqIrohClient {
    /// Creates a new `MoqIrohClient` instance.
    ///
    /// # Arguments
    /// - `endpoint`: The QUIC endpoint for network communication.
    /// - `gossip`: The gossip protocol instance for stream announcements.
    /// - `protocol`: The `MoqIroh` protocol handler that manages the engine.
    pub fn new(endpoint: Arc<Endpoint>, gossip: Arc<Gossip>, protocol: MoqIroh) -> Self {
        Self { protocol, endpoint, gossip }
    }

    /// Returns the node's unique identifier.
    pub fn node_id(&self) -> NodeId {
        self.endpoint.node_id()
    }

    /// Requests retransmission of a specific object from a stream.
    ///
    /// # Arguments
    /// - `publisher_id`: The ID of the publisher node.
    /// - `stream_id`: The unique identifier of the stream.
    /// - `namespace`: The namespace of the stream.
    /// - `sequence`: The sequence number to request.
    ///
    /// # Returns
    /// `Result<()>` on success or error.
    pub async fn request_object(
        &self,
        publisher_id: NodeId,
        stream_id: Uuid,
        namespace: String,
        sequence: u64
    ) -> Result<()> {
        // Use protocol to request retransmission
        self.protocol.request_object(publisher_id, stream_id, namespace, sequence).await
    }

    /// Monitors for new stream announcements from a specific sender.
    ///
    /// # Arguments
    /// - `sender_id`: The ID of the node to monitor for announcements.
    ///
    /// # Returns
    /// A stream of announcements that can be polled for new streams.
    pub async fn monitor_streams(
        &self,
        sender_id: NodeId
    ) -> Result<impl futures::Stream<Item = StreamAnnouncement>> {
        // Create a channel for announcements
        let (tx, rx) = mpsc::channel::<StreamAnnouncement>(32);

        // Subscribe to announcements via gossip
        let topic_id = blake3::hash(ANNOUNCE_TOPIC).into();
        info!("Subscribing to announcements via gossip on sub side");
        let mut topic = self.gossip.subscribe_and_join(topic_id, vec![sender_id]).await?;

        // Spawn a task to listen for announcements and send them to the channel
        tokio::spawn(async move {
            while let Some(Ok(event)) = topic.next().await {
                if let Event::Gossip(GossipEvent::Received(message)) = event {
                    if let Ok(announcement) = deserialize_announce(&message.content) {
                        // Filter to only include announcements from the specified sender
                        if announcement.sender_id == sender_id {
                            debug!(
                                "Received stream announcement: {} in {}",
                                announcement.stream_id,
                                announcement.namespace
                            );
                            if tx.send(announcement).await.is_err() {
                                // Receiver dropped, exit the loop
                                break;
                            }
                        }
                    }
                }
            }
        });

        // Return the receiver wrapped in a stream
        Ok(ReceiverStream::new(rx))
    }

    /// Unsubscribes from a stream.
    ///
    /// # Arguments
    /// - `node_id`: The ID of the node publishing the stream.
    /// - `stream_id`: The unique identifier of the stream.
    /// - `namespace`: The namespace of the stream.
    ///
    /// # Returns
    /// `Result<()>` on success or error.
    pub async fn unsubscribe(
        &self,
        node_id: NodeId,
        stream_id: Uuid,
        namespace: String
    ) -> Result<()> {
        // Use protocol to unsubscribe
        self.protocol.send_unsubscribe(node_id, stream_id, namespace).await
    }

    /// Terminates a stream connection.
    ///
    /// # Arguments
    /// - `node_id`: The ID of the node to terminate connection with.
    /// - `reason`: The reason code for termination.
    ///
    /// # Returns
    /// `Result<()>` on success or error.
    pub async fn terminate_stream(&self, node_id: NodeId, reason: u8) -> Result<()> {
        // Use protocol to terminate stream
        self.protocol.send_terminate(node_id, reason).await
    }

    /// Subscribes to connection state changes.
    ///
    /// # Returns
    /// A broadcast receiver for connection state updates.
    pub fn subscribe_to_state_changes(&self) -> broadcast::Receiver<ConnectionState> {
        // Forward to protocol
        self.protocol.subscribe_to_state_changes()
    }

    /// Creates a new video stream with initialization data.
    ///
    /// # Arguments
    /// - `namespace`: The namespace for the video stream.
    /// - `init`: The initialization segment with codec details.
    /// - `receiver_id`: The ID of the node to receive the stream.
    ///
    /// # Returns
    /// A tuple with the stream ID and a sender for video chunks.
    pub async fn publish_video_stream(
        &self,
        namespace: String,
        init: MediaInit
    ) -> Result<(Uuid, mpsc::Sender<VideoChunk>)> {
        // Use protocol to publish video stream
        self.protocol.publish_video_stream(namespace, init).await
    }

    /// Subscribes to a video stream.
    ///
    /// # Arguments
    /// - `announcement`: The announcement of the stream to subscribe to.
    ///
    /// # Returns
    /// A tuple containing receivers for the initialization segment and video chunks.
    pub async fn subscribe_to_video_stream(
        &self,
        announcement: StreamAnnouncement
    ) -> Result<(mpsc::Receiver<MediaInit>, mpsc::Receiver<VideoChunk>)> {
        info!(
            "Client: Directly subscribing to video stream {} in namespace {} from {}",
            announcement.stream_id,
            announcement.namespace,
            announcement.sender_id
        );

        // Add tracing for timeout handling
        let timeout_duration = Duration::from_secs(10);
        match
            tokio::time::timeout(
                timeout_duration,
                // Directly use the subscriber module instead of going through protocol
                subscriber::subscribe_to_video_stream(
                    self.endpoint.clone(),
                    self.protocol.engine().clone(), // Use the getter method
                    announcement.clone()
                )
            ).await
        {
            Ok(result) => {
                match result {
                    Ok(receivers) => {
                        info!(
                            "Client: Successfully subscribed to video stream {}",
                            announcement.stream_id
                        );
                        Ok(receivers)
                    }
                    Err(e) => {
                        error!(
                            "Client: Subscriber actor returned error for subscription to {}: {}",
                            announcement.stream_id,
                            e
                        );
                        Err(e)
                    }
                }
            }
            Err(_) => {
                error!(
                    "Client: Timeout after {}s waiting to subscribe to stream {}",
                    timeout_duration.as_secs(),
                    announcement.stream_id
                );
                bail!("Subscription request timed out")
            }
        }
    }

    /// Creates a new audio stream with initialization data.
    pub async fn publish_audio_stream(
        &self,
        namespace: String,
        init: AudioInit
    ) -> Result<(Uuid, mpsc::Sender<AudioChunk>)> {
        self.protocol.publish_audio_stream(namespace, init).await
    }

    /// Subscribes to an audio stream (using same announcement struct).
    pub async fn subscribe_to_audio_stream(
        &self,
        announcement: StreamAnnouncement
    ) -> Result<(mpsc::Receiver<AudioInit>, mpsc::Receiver<AudioChunk>)> {
        info!(
            "Client: Subscribing to audio stream {} in namespace {} from {}",
            announcement.stream_id,
            announcement.namespace,
            announcement.sender_id
        );

        let timeout_duration = Duration::from_secs(10);
        match
            tokio::time::timeout(
                timeout_duration,
                subscriber::subscribe_to_audio_stream(
                    self.endpoint.clone(),
                    self.protocol.engine().clone(),
                    announcement.clone()
                )
            ).await
        {
            Ok(res) => res,
            Err(_) => { bail!("Audio subscription timed out") }
        }
    }
}
