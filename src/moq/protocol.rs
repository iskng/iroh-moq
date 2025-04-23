use crate::moq::engine::{ ConnectionEvent, ConnectionState, MoqIrohEngine };
use crate::moq::proto::{
    deserialize_announce,
    deserialize_cancel,
    deserialize_heartbeat,
    deserialize_request,
    deserialize_subscribe,
    deserialize_terminate,
    deserialize_unsubscribe,
    serialize_announce,
    serialize_heartbeat,
    serialize_request,
    serialize_subscribe_ok,
    serialize_terminate,
    serialize_unsubscribe,
    AudioInit,
    MediaInit,
    MoqObject,
    StreamAnnouncement,
    VideoChunk,
    ALPN,
    ANNOUNCE_TOPIC,
    MEDIA_TYPE_INIT,
    MEDIA_TYPE_VIDEO,
    TYPE_CANCEL,
    TYPE_HEARTBEAT,
    TYPE_OBJECT,
    TYPE_REQUEST,
    TYPE_SUBSCRIBE,
    TYPE_SUBSCRIBE_OK,
    TYPE_TERMINATE,
    TYPE_UNSUBSCRIBE,
};
use anyhow::{ bail, Result };
use blake3;
use bytes::{ Buf, BytesMut };
use futures::{ future::BoxFuture, StreamExt };
use iroh::endpoint::{ Connection, RecvStream, SendStream };
use iroh::protocol::ProtocolHandler;
use iroh::{ Endpoint, NodeId };
use iroh_gossip::net::{ Event, Gossip, GossipEvent };
use std::sync::Arc;
use std::time::{ Duration, SystemTime, UNIX_EPOCH };
use tokio::io::AsyncWriteExt;
use tokio::sync::{ broadcast, mpsc };
use tracing::{ debug, error, info, trace, warn };
use uuid::Uuid;
use anyhow::anyhow;

use super::MoqIrohClient;
use crate::moq::subscriber;

// Protocol-specific constants
const STREAM_TYPE_CONTROL: u8 = 0x01;
const STREAM_TYPE_DATA: u8 = 0x02;
const STREAM_TYPE_HTTP: u8 = 0x03;

/// Configuration for the MOQ-Iroh service
#[derive(Clone, Debug)]
pub struct MoqIrohConfig {
    pub max_buffer_size: usize,
    pub heartbeat_interval: u64,
    pub enable_relay: bool,
}

impl Default for MoqIrohConfig {
    fn default() -> Self {
        Self {
            max_buffer_size: 1024 * 1024, // Example default
            heartbeat_interval: 5000, // Example default
            enable_relay: false, // Example default
        }
    }
}

/// Builder for the MoqIroh protocol
#[derive(Debug)]
pub struct Builder {
    config: Option<MoqIrohConfig>,
}

impl Builder {
    pub fn new() -> Self {
        Self { config: None }
    }

    pub fn config(mut self, config: MoqIrohConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub async fn spawn(self, endpoint: Endpoint, gossip: Arc<Gossip>) -> Result<MoqIroh> {
        let engine = MoqIrohEngine::new(endpoint, gossip).await?;
        // Apply config to engine if needed, or use it elsewhere
        Ok(MoqIroh::new(engine))
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self { config: None }
    }
}

/// The `MoqIroh` struct acts as a protocol handler for the `iroh-moq` project.
/// It wraps the `MoqIrohEngine` and implements the `ProtocolHandler` trait
/// to handle incoming connections. It also provides methods for the client
/// to interact with the engine, such as publishing and subscribing to streams.
#[derive(Debug, Clone)]
pub struct MoqIroh {
    /// A shared reference to the `MoqIrohEngine`, which manages the state
    /// and logic for stream handling.
    engine: Arc<MoqIrohEngine>,
}

impl MoqIroh {
    /// Creates a new `MoqIroh` instance with the given engine.
    ///
    /// # Arguments
    /// - `engine`: The `MoqIrohEngine` instance to wrap.
    ///
    /// # Returns
    /// A new `MoqIroh` instance.
    pub fn new(engine: MoqIrohEngine) -> Self {
        Self {
            engine: Arc::new(engine),
        }
    }

    /// Returns a builder for creating a new `MoqIroh` instance.
    ///
    /// # Returns
    /// A new `Builder` instance.
    pub fn builder() -> Builder {
        Builder::new()
    }

    /// Returns a client for interacting with the protocol.
    ///
    /// # Returns
    /// A new `MoqIrohClient` instance.
    pub fn client(&self) -> MoqIrohClient {
        MoqIrohClient::new(
            self.engine.endpoint().clone(),
            self.engine.gossip().clone(),
            self.clone()
        )
    }

    /// Subscribes to an existing stream.
    ///
    /// # Arguments
    /// - `publisher_id`: The ID of the node publishing the stream.
    /// - `stream_id`: The ID of the stream to subscribe to.
    /// - `namespace`: The namespace of the stream.
    /// - `start_sequence`: The sequence number to start from.
    /// - `group_id`: The group ID to subscribe to.
    /// - `priority`: The priority of the subscription.
    ///
    /// # Returns
    /// A receiver for objects from the stream.
    pub async fn subscribe_to_stream(
        &self,
        publisher_id: NodeId,
        stream_id: Uuid,
        namespace: String,
        start_sequence: u64,
        group_id: u32,
        priority: u8
    ) -> Result<mpsc::Receiver<MoqObject>> {
        // Create a channel for this subscription
        let (_tx, rx) = mpsc::channel(512);

        // Register with engine (which no longer returns a channel)
        self.engine.subscribe_to_stream(
            publisher_id,
            stream_id,
            namespace.clone(),
            start_sequence,
            group_id,
            priority
        ).await?;

        // Since we're not getting a channel from the engine anymore,
        // we'll create our own and return it to maintain API compatibility
        Ok(rx)
    }

    /// Requests a specific object from a publisher.
    ///
    /// # Arguments
    /// - `publisher_id`: The ID of the publisher node.
    /// - `stream_id`: The ID of the stream.
    /// - `namespace`: The namespace of the stream.
    /// - `sequence`: The sequence number of the object to request.
    ///
    /// # Returns
    /// `Ok(())` if the request was sent successfully.
    pub async fn request_object(
        &self,
        publisher_id: NodeId,
        stream_id: Uuid,
        namespace: String,
        sequence: u64
    ) -> Result<()> {
        debug!(
            "Requesting object: stream={} sequence={} from {}",
            stream_id,
            sequence,
            publisher_id
        );

        // Connect to the publisher if not already connected
        let conn = self.engine.endpoint().connect(publisher_id, ALPN).await?;

        // Open control stream
        let (mut send, _) = conn.open_bi().await?;
        send.write_all(&[STREAM_TYPE_CONTROL]).await?;

        // Send request message
        let request_msg = serialize_request(stream_id, &namespace, sequence);
        send.write_all(&request_msg).await?;

        debug!("Sent request for object sequence={} in stream={}", sequence, stream_id);

        Ok(())
    }

    /// Sends an unsubscribe message to a node and removes the stream from tracking.
    ///
    /// # Arguments
    /// - `node_id`: The ID of the node to unsubscribe from.
    /// - `stream_id`: The ID of the stream to unsubscribe from.
    /// - `namespace`: The namespace of the stream.
    ///
    /// # Returns
    /// `Ok(())` if the unsubscribe was successful.
    pub async fn send_unsubscribe(
        &self,
        node_id: NodeId,
        stream_id: Uuid,
        namespace: String
    ) -> Result<()> {
        debug!(
            "Unsubscribing from stream={} namespace={} on node={}",
            stream_id,
            namespace,
            node_id
        );

        // Connect to the node
        let conn = self.engine.endpoint().connect(node_id, ALPN).await?;

        // Open control stream
        let (mut send, _) = conn.open_bi().await?;
        send.write_all(&[STREAM_TYPE_CONTROL]).await?;

        // Send unsubscribe message
        let unsubscribe_msg = serialize_unsubscribe(stream_id, &namespace);
        send.write_all(&unsubscribe_msg).await?;

        // Remove from engine tracking
        self.engine.unsubscribe(stream_id, namespace.clone(), node_id).await?;

        debug!("Unsubscribed from stream={} namespace={}", stream_id, namespace);

        Ok(())
    }

    /// Sends a terminate message to a node to close the connection.
    ///
    /// # Arguments
    /// - `node_id`: The ID of the node to terminate.
    /// - `reason`: The reason code for termination.
    ///
    /// # Returns
    /// `Ok(())` if the terminate message was sent successfully.
    pub async fn send_terminate(&self, node_id: NodeId, reason: u8) -> Result<()> {
        debug!("Terminating connection to {} with reason {}", node_id, reason);

        // Connect to the node
        let conn = self.engine.endpoint().connect(node_id, ALPN).await?;

        // Open control stream
        let (mut send, _) = conn.open_bi().await?;
        send.write_all(&[STREAM_TYPE_CONTROL]).await?;

        // Send terminate message
        let terminate_msg = serialize_terminate(reason);
        send.write_all(&terminate_msg).await?;

        // Close connection
        conn.close((0u32).into(), &[]);

        debug!("Terminated connection to {}", node_id);

        Ok(())
    }

    /// Registers a stream with the engine.
    ///
    /// # Arguments
    /// - `stream_id`: The ID of the stream to register.
    /// - `namespace`: The namespace of the stream.
    ///
    /// # Returns
    /// A sender for publishing objects to the stream.
    pub async fn register_stream(
        &self,
        stream_id: Uuid,
        namespace: String
    ) -> mpsc::Sender<MoqObject> {
        self.engine.register_stream(stream_id, namespace).await
    }

    /// Subscribes to connection state changes from the engine.
    ///
    /// # Returns
    /// A receiver for connection state changes.
    pub fn subscribe_to_state_changes(&self) -> broadcast::Receiver<ConnectionState> {
        self.engine.subscribe_to_state_changes()
    }

    /// Gets a stream announcement from a topic.
    ///
    /// # Arguments
    /// - `stream_id`: The ID of the stream to get the announcement for.
    ///
    /// # Returns
    /// The stream announcement if found.
    pub async fn get_stream_announcement(&self, stream_id: Uuid) -> Result<StreamAnnouncement> {
        // Subscribe to the announcement topic
        let topic_id = blake3::hash(ANNOUNCE_TOPIC).into();
        let mut topic = self.engine
            .gossip()
            .subscribe_and_join(topic_id, vec![self.engine.endpoint().node_id()]).await?;

        let timeout = Duration::from_secs(5);
        let start = SystemTime::now();

        while SystemTime::now().duration_since(start)? < timeout {
            if let Some(Ok(event)) = topic.next().await {
                if let Event::Gossip(GossipEvent::Received(message)) = event {
                    match deserialize_announce(&message.content) {
                        Ok(announcement) => {
                            if announcement.stream_id == stream_id {
                                return Ok(announcement);
                            }
                        }
                        Err(e) => {
                            debug!("Failed to deserialize announcement: {}", e);
                        }
                    }
                }
            }
        }

        bail!("Announcement for stream {} not found", stream_id)
    }

    /// Signals a connection event to the engine.
    ///
    /// # Arguments
    /// - `event`: The connection event to signal.
    pub async fn signal_connection_event(&self, event: ConnectionEvent) {
        self.engine.signal_connection_event(event).await;
    }

    /// Publishes a video stream.
    ///
    /// # Arguments
    /// - `namespace`: The namespace of the stream.
    /// - `init`: The initialization segment data.
    /// - `receiver_id`: The ID of the node to receive the stream.
    ///
    /// # Returns
    /// A tuple containing the stream ID and a sender for video chunks.
    pub async fn publish_video_stream(
        &self,
        namespace: String,
        init: MediaInit
    ) -> Result<(Uuid, mpsc::Sender<VideoChunk>)> {
        // Create a single stream ID that will be used for both announcement and data
        let stream_id = Uuid::new_v4();
        debug!("Creating video stream with ID: {} in namespace: {}", stream_id, namespace);

        // First register the stream with the engine using the stream ID
        // This ensures the stream exists before any subscribers try to connect
        debug!("Registering stream {} with engine", stream_id);
        let object_tx = self.engine.register_stream(stream_id, namespace.clone()).await;

        // Create and send initialization segment immediately
        let init_object = MoqObject {
            name: format!("{}/init", namespace),
            sequence: 0,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64,
            group_id: MEDIA_TYPE_INIT as u32,
            priority: 255, // Highest priority for init segment
            data: init.init_segment.clone(),
        };

        // Send init segment
        if let Err(e) = object_tx.send(init_object).await {
            error!("Failed to send init segment: {}", e);
            bail!("Failed to send initialization segment");
        }

        // Create the announcement after registering the stream
        let announcement = StreamAnnouncement {
            stream_id, // Use the same stream ID
            sender_id: self.engine.endpoint().node_id(),
            namespace: namespace.clone(),
            codec: init.codec.clone(),
            resolution: (init.width, init.height),
            framerate: init.frame_rate as u32,
            bitrate: init.bitrate,
            timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros() as u64,
            relay_ids: vec![],
        };

        // After the stream is registered, announce it via gossip
        debug!("Announcing stream {} via gossip", stream_id);
        let topic_id = blake3::hash(ANNOUNCE_TOPIC).into();
        let topic = self.engine.gossip().subscribe_and_join(topic_id, vec![]).await?;

        // Serialize and broadcast the announcement
        let msg = serialize_announce(&announcement)?;

        if let Err(e) = topic.broadcast(msg.clone().into()).await {
            error!("Failed to broadcast announcement: {}", e);
        } else {
            debug!("Announcement broadcast sent for stream {}", stream_id);
        }

        // Small delay between retries
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Create video chunk channel
        let (chunk_tx, mut chunk_rx) = mpsc::channel::<VideoChunk>(512);

        // Handle converting chunks to objects
        let object_tx_clone = object_tx.clone();
        let namespace_clone = namespace.clone();
        tokio::spawn(async move {
            let mut sequence = 1u64; // Start at 1 since init segment is 0

            // Process each chunk individually for lowest latency
            debug!("Starting video chunk processing for stream {}", stream_id);
            while let Some(chunk) = chunk_rx.recv().await {
                let object = MoqObject {
                    name: format!("{}/chunk_{}", namespace_clone, sequence),
                    sequence,
                    timestamp: chunk.timestamp,
                    group_id: MEDIA_TYPE_VIDEO as u32,
                    priority: if chunk.is_keyframe {
                        255
                    } else {
                        128
                    },
                    data: chunk.data,
                };

                sequence += 1;

                // Send each object immediately for lowest latency
                if let Err(e) = object_tx_clone.send(object).await {
                    error!("Failed to send video chunk: {}", e);
                    return;
                }
            }

            debug!("Video chunk processing complete for stream {}", stream_id);
        });

        info!("Video stream {} successfully published", stream_id);
        Ok((stream_id, chunk_tx))
    }

    /// Publishes an audio stream.
    /// Returns (stream_id, sender for audio chunks)
    pub async fn publish_audio_stream(
        &self,
        namespace: String,
        init: AudioInit
    ) -> Result<(Uuid, mpsc::Sender<MoqObject>)> {
        // Generate a stream_id (reuse video pattern)
        let stream_id = Uuid::new_v4();
        let normalized_ns = namespace.clone();

        // Register stream first, get the sender to the engine's stream actor
        let engine_object_tx = self.engine.register_stream(stream_id, normalized_ns.clone()).await;

        // Send init segment as MoqObject via the engine sender
        let init_object = MoqObject {
            name: format!("{}/audio_init", normalized_ns),
            sequence: 0,
            timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros() as u64,
            group_id: MEDIA_TYPE_INIT as u32,
            priority: 255,
            data: init.init_segment.clone(),
        };

        if let Err(e) = engine_object_tx.send(init_object).await {
            error!("Failed to send audio init segment via engine: {}", e);
            bail!("Failed to send audio initialization segment");
        }

        // Broadcast announcement (same as before)
        let announcement = StreamAnnouncement {
            stream_id,
            sender_id: self.engine.endpoint().node_id(),
            namespace: normalized_ns.clone(),
            codec: init.codec.clone(),
            resolution: (0, 0),
            framerate: 0,
            bitrate: init.bitrate,
            timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros() as u64,
            relay_ids: vec![],
        };
        let topic_id = blake3::hash(ANNOUNCE_TOPIC).into();
        let topic = self.engine.gossip().subscribe_and_join(topic_id, vec![]).await?;
        let msg = serialize_announce(&announcement)?;
        let _ = topic.broadcast(msg.into()).await;

        // Create a NEW channel specifically for the application (stream_audio) to send objects
        let (app_object_tx, mut app_object_rx) = mpsc::channel::<MoqObject>(512);

        // Clone the sender that goes to the engine's stream actor
        let engine_object_tx_clone = engine_object_tx.clone();

        // Spawn a task that bridges the app channel to the engine channel
        tokio::spawn(async move {
            // Read objects from the application (sent by stream_audio)
            while let Some(object) = app_object_rx.recv().await {
                // Send the object to the engine's stream actor
                if let Err(e) = engine_object_tx_clone.send(object).await {
                    error!(
                        "Failed to forward MoqObject from app to engine for stream {}: {}",
                        stream_id,
                        e
                    );
                    break; // Stop forwarding if engine channel fails
                }
            }
            // When app_object_rx closes (because stream_audio drops app_object_tx),
            // this task terminates. The engine_object_tx_clone is dropped here.
            // This signals the engine's stream actor that this particular source has finished.
            info!("Application object forwarding loop finished for stream {}", stream_id);
        });

        // Return the sender side of the NEW application channel
        Ok((stream_id, app_object_tx))
    }

    /// Subscribes to a video stream from the announcement
    ///
    /// # Arguments
    /// - `announcement`: The stream announcement to subscribe to.
    ///
    /// # Returns
    /// A tuple of receivers for initialization segments and video chunks.
    pub async fn subscribe_to_video_stream(
        &self,
        announcement: StreamAnnouncement
    ) -> Result<(mpsc::Receiver<MediaInit>, mpsc::Receiver<Option<VideoChunk>>)> {
        // Use the new subscriber actor implementation
        subscriber::subscribe_to_video_stream(
            self.engine.endpoint().clone(),
            self.engine.clone(),
            announcement
        ).await
    }

    /// Handles a control stream by processing incoming control messages.
    ///
    /// # Arguments
    /// - `send`: The send stream for the control connection.
    /// - `recv`: The receive stream for the control connection.
    /// - `remote_id`: The ID of the remote node.
    /// - `conn`: The connection to the remote node.
    ///
    /// # Returns
    /// `Ok(())` if the stream was handled successfully.
    async fn handle_control_stream(
        &self,
        mut send: SendStream,
        mut recv: RecvStream,
        remote_id: NodeId,
        conn: Connection
    ) -> Result<()> {
        let mut buffer = BytesMut::new();

        loop {
            match recv.read_chunk(65535, false).await {
                Ok(Some(chunk)) => {
                    buffer.extend(chunk.bytes);
                    self.process_control_messages(&mut buffer, &mut send, remote_id, &conn).await?;
                }
                Ok(None) => {
                    info!("Control stream closed gracefully");
                    return Ok(());
                }
                Err(e) => {
                    error!("Error reading from control stream: {}", e);
                    return Err(e.into());
                }
            }
        }
    }

    /// Processes control messages from a buffer.
    ///
    /// # Arguments
    /// - `buffer`: The buffer containing the messages.
    /// - `send`: The send stream for sending responses.
    /// - `remote_id`: The ID of the remote node.
    /// - `conn`: The connection to the remote node.
    ///
    /// # Returns
    /// `Ok(())` if the messages were processed successfully.
    async fn process_control_messages(
        &self,
        buffer: &mut BytesMut,
        send: &mut SendStream,
        remote_id: NodeId,
        conn: &Connection
    ) -> Result<()> {
        while buffer.len() >= 1 {
            let msg_type = buffer[0];

            // Check if we have a complete message
            let complete = match msg_type {
                TYPE_SUBSCRIBE => {
                    if buffer.len() < 3 {
                        break;
                    }
                    let ns_len = (((buffer[1] as u16) << 8) | (buffer[2] as u16)) as usize;
                    buffer.len() >= 3 + ns_len + 29
                }
                TYPE_TERMINATE => buffer.len() >= 2,
                TYPE_HEARTBEAT => buffer.len() >= 9,
                TYPE_REQUEST => buffer.len() >= 25,
                TYPE_UNSUBSCRIBE => buffer.len() >= 2 && buffer.len() >= 2 + (buffer[1] as usize),
                TYPE_CANCEL => buffer.len() >= 9,
                TYPE_SUBSCRIBE_OK => buffer.len() >= 17,
                TYPE_OBJECT => {
                    error!("Received object message on control stream");
                    buffer.advance(1);
                    continue;
                }
                _ => {
                    error!("Skipping unknown message type: {}", msg_type);
                    buffer.advance(1);
                    continue;
                }
            };

            if !complete {
                break;
            }

            match msg_type {
                TYPE_SUBSCRIBE => {
                    self.handle_subscribe_message(buffer, send, remote_id, conn).await?;
                }
                TYPE_SUBSCRIBE_OK => self.handle_subscribe_ok_message(buffer, remote_id).await?,
                TYPE_TERMINATE => self.handle_terminate_message(buffer).await?,
                TYPE_HEARTBEAT => {
                    self.handle_heartbeat_message(buffer, send, remote_id).await?;
                }
                TYPE_REQUEST => self.handle_request_message(buffer, remote_id).await?,
                TYPE_UNSUBSCRIBE => self.handle_unsubscribe_message(buffer).await?,
                TYPE_CANCEL => self.handle_cancel_message(buffer).await?,
                _ => {
                    error!("Unknown message type after validation: {}", msg_type);
                    buffer.advance(1);
                }
            }
        }

        Ok(())
    }

    /// Handles a SUBSCRIBE message.
    ///
    /// # Arguments
    /// - `buffer`: The buffer containing the message.
    /// - `send`: The send stream for sending responses.
    /// - `remote_id`: The ID of the remote node.
    /// - `conn`: The connection to the remote node.
    ///
    /// # Returns
    /// `Ok(())` if the message was handled successfully.
    async fn handle_subscribe_message(
        &self,
        buffer: &mut BytesMut,
        send: &mut SendStream,
        remote_id: NodeId,
        _conn: &Connection
    ) -> Result<()> {
        let (stream_id, namespace, start_sequence, group_id, priority) =
            deserialize_subscribe(buffer)?;

        // Don't manually clean the namespace here - the engine will normalize it

        debug!(
            "Received SUBSCRIBE message for stream {} namespace {} from {}",
            stream_id,
            namespace,
            remote_id
        );

        // Check if we're publishing this stream
        let stream_exists = match
            self.engine.subscribe_to_stream(
                remote_id,
                stream_id,
                namespace.clone(),
                start_sequence,
                group_id,
                priority
            ).await
        {
            Ok(_) => {
                info!(
                    "Successfully registered subscription for stream: {stream_id} in namespace: {namespace}"
                );
                true
            }
            Err(e) => {
                warn!(
                    "Failed to register subscription for stream: {stream_id} in namespace: {namespace}: {e}"
                );
                false
            }
        };

        if stream_exists {
            // Send SUBSCRIBE_OK response
            let response = serialize_subscribe_ok(stream_id);

            // Send the length of the response as a 4-byte big-endian integer
            send
                .write_all(&(response.len() as u32).to_be_bytes()).await
                .map_err(|e| anyhow::anyhow!("Failed to send SUBSCRIBE_OK length: {}", e))?;

            // Send the actual response
            send
                .write_all(&response).await
                .map_err(|e| anyhow::anyhow!("Failed to send SUBSCRIBE_OK response: {}", e))?;

            send
                .flush().await
                .map_err(|e| anyhow::anyhow!("Failed to flush SUBSCRIBE_OK response: {}", e))?;

            info!("Sent SUBSCRIBE_OK response for stream: {stream_id} in namespace: {namespace}");
        } else {
            // Don't rely on alternative namespace formats - the engine's normalize_namespace should handle this
            warn!("Stream not found: {stream_id} with namespace: {namespace}");
        }

        Ok(())
    }

    /// Handles a SUBSCRIBE_OK message.
    ///
    /// # Arguments
    /// - `buffer`: The buffer containing the message.
    /// - `remote_id`: The ID of the remote node.
    ///
    /// # Returns
    /// `Ok(())` if the message was handled successfully.
    async fn handle_subscribe_ok_message(
        &self,
        buffer: &mut BytesMut,
        remote_id: NodeId
    ) -> Result<()> {
        debug!("Deserializing SUBSCRIBE_OK message from {}", remote_id);
        buffer.advance(1);
        let mut uuid_bytes = [0u8; 16];
        uuid_bytes.copy_from_slice(&buffer[..16]);
        buffer.advance(16);
        let stream_id = Uuid::from_bytes_le(uuid_bytes);

        info!("Received SUBSCRIBE_OK for stream {} from {}", stream_id, remote_id);

        // Notify the engine that we received SUBSCRIBE_OK
        debug!("Notifying engine about SUBSCRIBE_OK for connection from {}", remote_id);
        match self.engine.notify_subscribe_ok(remote_id).await {
            Ok(_) => info!("Successfully notified engine about SUBSCRIBE_OK from {}", remote_id),
            Err(e) => error!("Failed to notify engine about SUBSCRIBE_OK: {}", e),
        }

        Ok(())
    }

    /// Handles a TERMINATE message.
    ///
    /// # Arguments
    /// - `buffer`: The buffer containing the message.
    ///
    /// # Returns
    /// `Ok(())` if the message was handled successfully.
    async fn handle_terminate_message(&self, buffer: &mut BytesMut) -> Result<()> {
        if let Ok(reason) = deserialize_terminate(buffer) {
            info!("Received terminate with reason: {}", reason);
        }
        Ok(())
    }

    /// Handles a HEARTBEAT message.
    ///
    /// # Arguments
    /// - `buffer`: The buffer containing the message.
    /// - `send`: The send stream for sending responses.
    /// - `remote_id`: The ID of the remote node.
    ///
    /// # Returns
    /// `Ok(())` if the message was handled successfully.
    async fn handle_heartbeat_message(
        &self,
        buffer: &mut BytesMut,
        send: &mut SendStream,
        remote_id: NodeId
    ) -> Result<()> {
        if let Ok(timestamp) = deserialize_heartbeat(buffer) {
            debug!("Responding to heartbeat from {} with timestamp: {}", remote_id, timestamp);
            let response = serialize_heartbeat(timestamp);
            send.write_all(&response).await?;
        }
        Ok(())
    }

    /// Handles a REQUEST message.
    ///
    /// # Arguments
    /// - `buffer`: The buffer containing the message.
    /// - `remote_id`: The ID of the remote node.
    ///
    /// # Returns
    /// `Ok(())` if the message was handled successfully.
    async fn handle_request_message(&self, buffer: &mut BytesMut, remote_id: NodeId) -> Result<()> {
        if let Ok((stream_id, namespace, sequence)) = deserialize_request(buffer) {
            debug!(
                "Received retransmission request for sequence: {} in stream: {}",
                sequence,
                stream_id
            );

            // Forward to engine to handle retransmission
            self.engine.request_retransmission(stream_id, namespace, remote_id, sequence).await?;
        }
        Ok(())
    }

    /// Handles an UNSUBSCRIBE message.
    ///
    /// # Arguments
    /// - `buffer`: The buffer containing the message.
    ///
    /// # Returns
    /// `Ok(())` if the message was handled successfully.
    async fn handle_unsubscribe_message(&self, buffer: &mut BytesMut) -> Result<()> {
        if let Ok((stream_id, namespace)) = deserialize_unsubscribe(buffer) {
            debug!("Received unsubscribe for stream: {} in namespace: {}", stream_id, namespace);
            // No action needed, the connection will be cleaned up
        }
        Ok(())
    }

    /// Handles a CANCEL message.
    ///
    /// # Arguments
    /// - `buffer`: The buffer containing the message.
    ///
    /// # Returns
    /// `Ok(())` if the message was handled successfully.
    async fn handle_cancel_message(&self, buffer: &mut BytesMut) -> Result<()> {
        if let Ok(sequence) = deserialize_cancel(buffer) {
            debug!("Received cancel for sequence: {}", sequence);
            // No action needed, just acknowledge
        }
        Ok(())
    }

    /// Handles a data stream: initial connection setup and ongoing data processing.
    ///
    /// # Arguments
    /// - `stream_id`: The ID of the stream to register.
    /// - `namespace`: The namespace of the stream.
    /// - `recv`: The receive stream for the data connection.
    /// - `send`: The send stream for the data connection.
    /// - `engine`: A reference to the engine.
    /// - `remote_id`: The ID of the remote node.
    ///
    /// # Returns
    /// `Result<()>` if the stream was handled successfully.
    async fn handle_data_stream(
        _stream_id: Uuid,
        _namespace: String,
        mut recv: RecvStream,
        send: SendStream,
        engine: Arc<MoqIrohEngine>,
        remote_id: NodeId
    ) -> Result<()> {
        // Read the stream ID from the data stream
        let mut stream_id_bytes = [0u8; 16];
        if let Err(e) = recv.read_exact(&mut stream_id_bytes).await {
            error!("Failed to read stream ID from data stream: {}", e);
            return Err(anyhow::anyhow!("Failed to read stream ID: {}", e));
        }

        let stream_id = Uuid::from_bytes_le(stream_id_bytes);
        debug!("Read stream ID from data stream: {}", stream_id);

        // Read namespace length as little-endian u16
        let mut namespace_len_bytes = [0u8; 2];
        if let Err(e) = recv.read_exact(&mut namespace_len_bytes).await {
            error!("Failed to read namespace length from data stream: {}", e);
            return Err(anyhow::anyhow!("Failed to read namespace length: {}", e));
        }

        let namespace_len = u16::from_le_bytes(namespace_len_bytes) as usize;
        debug!("Read namespace length from data stream: {}", namespace_len);

        // Read namespace
        let mut namespace_bytes = vec![0u8; namespace_len];
        if let Err(e) = recv.read_exact(&mut namespace_bytes).await {
            error!("Failed to read namespace from data stream: {}", e);
            return Err(anyhow::anyhow!("Failed to read namespace: {}", e));
        }

        let namespace = match String::from_utf8(namespace_bytes) {
            Ok(ns) => ns,
            Err(e) => {
                error!("Failed to decode namespace as UTF-8: {}", e);
                return Err(anyhow::anyhow!("Failed to decode namespace: {}", e));
            }
        };
        debug!("Read namespace from data stream: {}", namespace);

        info!(
            "Registering data stream with engine for stream_id: {} namespace: {}",
            stream_id,
            namespace
        );

        // Now register the SendStream with the engine to send objects to the subscriber
        // Check the engine result to handle cases where we're not the publisher
        match engine.set_data_stream(stream_id, namespace.clone(), remote_id, send).await {
            Ok(_) => {
                info!("Successfully registered data stream for stream_id: {}", stream_id);

                // Keep the connection open until the remote closes it to drain incoming data
                info!("Keeping connection open for stream_id: {}", stream_id);
                let buf = vec![0u8; 1024];
                loop {
                    match recv.read_chunk(buf.len(), false).await {
                        Ok(Some(chunk)) => {
                            trace!("Read {} bytes from data stream", chunk.bytes.len());
                        }
                        Ok(None) => {
                            debug!("Remote closed data stream connection for stream_id: {}", stream_id);
                            break;
                        }
                        Err(e) => {
                            debug!("Error reading from data stream: {}", e);
                            break;
                        }
                    }
                }

                Ok(())
            }
            Err(e) => {
                // This is likely because we're not the publisher for this stream
                // We can quietly log this and return without an error
                debug!("We are not the publisher for stream {}: {}", stream_id, e);
                Ok(())
            }
        }
    }

    /// Get the current connection state
    pub async fn connection_state(&self) -> ConnectionState {
        let mut state_rx = self.engine.subscribe_to_state_changes();
        // Try to get the latest state if available, otherwise return WaitingForPeer
        match state_rx.try_recv() {
            Ok(state) => state,
            Err(_) => ConnectionState::WaitingForPeer,
        }
    }

    /// Returns a reference to the underlying engine
    ///
    /// # Returns
    /// A reference to the `MoqIrohEngine` instance.
    pub fn engine(&self) -> &Arc<MoqIrohEngine> {
        &self.engine
    }
}

// --------------------------------------------------------------------
// Helper function to handle incoming HTTP proxy requests
// --------------------------------------------------------------------
async fn handle_http_stream(mut send: SendStream, mut recv: RecvStream) -> anyhow::Result<()> {
    // 3a. read the entire HTTP request the subscriber sent
    let mut req_bytes = Vec::new();
    // Limit request size to avoid DoS
    let mut total_read = 0;
    let max_request_size = 16 * 1024; // e.g., 16 KiB

    while let Some(chunk) = recv.read_chunk(1024, false).await? {
        total_read += chunk.bytes.len();
        if total_read > max_request_size {
            bail!("HTTP request exceeds maximum size of {} bytes", max_request_size);
        }
        req_bytes.extend_from_slice(&chunk.bytes);
    }

    if req_bytes.is_empty() {
        bail!("Received empty HTTP request");
    }

    let req_str = String::from_utf8(req_bytes)?;
    info!("Received HTTP request:\n{}", req_str);

    // 3b. Very basic parser – just extract path from the first line
    let path = req_str
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .ok_or_else(|| anyhow!("malformed request line"))?;

    // Sanitize path to prevent directory traversal
    let sanitized_path = path.trim_start_matches('/');
    if sanitized_path.contains("..") {
        bail!("invalid path contains '..'");
    }

    // 3c. Serve local file from a 'www' subdirectory
    let file_path = format!("www/{}", sanitized_path);
    info!("Attempting to serve file: {}", file_path);

    match tokio::fs::read(&file_path).await {
        Ok(body) => {
            // 3d. craft a minimal HTTP/1.1 response
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", // Added Connection: close
                body.len()
            );
            send.write_all(header.as_bytes()).await?;
            send.write_all(&body).await?;
            info!("Successfully sent file: {}", file_path);
        }
        Err(e) => {
            // Handle file not found or other errors
            error!("Failed to read file {}: {}", file_path, e);
            let (status_code, message) = if e.kind() == std::io::ErrorKind::NotFound {
                ("404 Not Found", "File not found.")
            } else {
                ("500 Internal Server Error", "Failed to read file.")
            };
            let body = message.as_bytes();
            let header = format!(
                "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                status_code,
                body.len()
            );
            send.write_all(header.as_bytes()).await?;
            send.write_all(body).await?;
        }
    }

    // 3e. Finish the stream
    send.finish()?;
    debug!("Finished sending HTTP response for {}", path);
    Ok(())
}

impl ProtocolHandler for MoqIroh {
    /// Handles incoming connections by accepting them and setting up control/data streams.
    ///
    /// # Arguments
    /// - `conn`: The incoming connection to handle.
    ///
    /// # Returns
    /// A future that resolves to Ok if the connection was handled successfully, or an error.
    fn accept(&self, conn: Connection) -> BoxFuture<'static, Result<()>> {
        let protocol = self.clone();
        Box::pin(async move {
            let remote_id = conn.remote_node_id()?;
            info!("Accepted MOQ-Iroh connection from: {}", remote_id.to_string());

            // Signal peer connection
            protocol.signal_connection_event(ConnectionEvent::PeerConnected(remote_id)).await;

            // Handle incoming bi-directional streams
            while let Ok((send, mut recv)) = conn.accept_bi().await {
                let mut stream_type_buf = [0u8; 1];
                match recv.read_exact(&mut stream_type_buf).await {
                    Ok(_) => {
                        let stream_type = stream_type_buf[0];
                        let protocol_clone = protocol.clone();
                        let conn_clone = conn.clone(); // Clone conn for use inside the match arms

                        match stream_type {
                            STREAM_TYPE_CONTROL => {
                                info!("Accepted control stream from {}", remote_id);
                                tokio::spawn(async move {
                                    if
                                        let Err(e) = protocol_clone.handle_control_stream(
                                            send,
                                            recv,
                                            remote_id,
                                            conn_clone // Pass the cloned connection
                                        ).await
                                    {
                                        error!("Control stream error: {:?}", e);
                                    }
                                });
                            }
                            STREAM_TYPE_DATA => {
                                info!("Accepted data stream from {}", remote_id);
                                let protocol_for_data = protocol_clone.clone();
                                // Create a UUID for the stream - in a real implementation this would be
                                // determined from the data stream identification information
                                let default_stream_id = Uuid::new_v4();
                                // Use a default namespace
                                let default_namespace = "default".to_string();

                                tokio::spawn(async move {
                                    if
                                        let Err(e) = Self::handle_data_stream(
                                            default_stream_id,
                                            default_namespace,
                                            recv,
                                            send,
                                            protocol_for_data.engine, // Pass the engine Arc directly
                                            remote_id
                                        ).await
                                    {
                                        error!("Data stream connection error: {:?}", e);
                                    }
                                });
                            }
                            STREAM_TYPE_HTTP => {
                                info!("Accepted HTTP proxy stream from {}", remote_id);
                                // No need to clone protocol_clone here, already cloned above
                                tokio::spawn(async move {
                                    if let Err(e) = handle_http_stream(send, recv).await {
                                        error!("HTTP proxy stream error: {}", e);
                                    }
                                });
                            }
                            unknown => {
                                error!("Unknown stream type: {}", unknown);
                                // Optionally close the stream immediately
                                // let _ = send.finish();
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to read stream type: {}", e);
                        // Break the loop if we can't even read the stream type
                        break;
                    }
                }
            }

            info!("Finished handling connection from {}", remote_id); // Log when loop exits
            Ok(())
        })
    }
}
