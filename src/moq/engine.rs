use crate::moq::proto::{
    deserialize_cancel,
    deserialize_heartbeat,
    deserialize_object,
    deserialize_request,
    deserialize_subscribe,
    deserialize_terminate,
    deserialize_unsubscribe,
    serialize_announce,
    serialize_heartbeat,
    serialize_object,
    serialize_request,
    serialize_subscribe,
    serialize_terminate,
    serialize_unsubscribe,
    serialize_subscribe_ok,
    MoqObject,
    StreamAnnouncement,
    ALPN,
    ANNOUNCE_TOPIC,
    REASON_ERROR,
    REASON_NORMAL,
    TYPE_CANCEL,
    TYPE_HEARTBEAT,
    TYPE_OBJECT,
    TYPE_REQUEST,
    TYPE_SUBSCRIBE,
    TYPE_TERMINATE,
    TYPE_UNSUBSCRIBE,
    TYPE_SUBSCRIBE_OK,
    MEDIA_TYPE_INIT,
    MEDIA_TYPE_VIDEO,
    MediaInit,
    VideoChunk,
};
use iroh::Endpoint;
use iroh::endpoint::{ Connection, SendStream, RecvStream, Connecting };
use iroh::NodeId;
use iroh_gossip::net::Gossip;
use anyhow::{ Result, bail };
use tokio::sync::{ mpsc, Mutex, broadcast };
use std::sync::Arc;
use tracing::{ info, error, debug, warn, trace };
use bytes::{ BytesMut, Buf, BufMut };
use std::collections::{ HashMap, VecDeque };
use dashmap::DashMap;
use uuid::Uuid;
use std::time::{ Duration, SystemTime, UNIX_EPOCH };
use std::sync::atomic::{ AtomicU64, Ordering };
use tokio::io::AsyncWriteExt;

// Constants
const HEARTBEAT_INTERVAL: u64 = 5; // Seconds between heartbeats
const HEARTBEAT_TIMEOUT: u64 = 15; // Seconds before timeout
const SLIDING_WINDOW_SIZE: usize = 100; // Max objects in retransmission buffer
const MIN_HEARTBEAT_SPACING: u64 = 1; // Minimum seconds between heartbeat responses
const STREAM_TYPE_CONTROL: u8 = 0x01;
const STREAM_TYPE_DATA: u8 = 0x02;
const TYPE_READY: u8 = 0x07; // New message type for stream readiness

// Connection state for the MOQ-Iroh protocol
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    WaitingForPeer,
    Connected(iroh::NodeId),
    Ready {
        node_id: iroh::NodeId,
        stream_id: Uuid,
        namespace: String,
    },
}

// Simplified connection event enum
#[derive(Debug, Clone)]
pub enum ConnectionEvent {
    PeerConnected(NodeId),
    StreamReady {
        node_id: NodeId,
        stream_id: Uuid,
        namespace: String,
    },
}

/// Commands that can be sent to stream actors
#[derive(Debug)]
enum StreamCommand {
    PublishObject(MoqObject),
    Subscribe {
        node_id: NodeId,
        group_id: u32,
    },
    SetDataStream {
        node_id: NodeId,
        stream: SendStream,
    },
    Unsubscribe(NodeId),
    RequestRetransmission {
        node_id: NodeId,
        sequence: u64,
    },
}

// The core engine that handles the MOQ-Iroh protocol operations
#[derive(Debug, Clone)]
pub struct MoqIrohEngine {
    endpoint: Arc<Endpoint>,
    gossip: Arc<Gossip>,
    stream_actors: Arc<DashMap<(Uuid, String), mpsc::Sender<StreamCommand>>>,
    connection_state: Arc<Mutex<ConnectionState>>,
    state_tx: Arc<broadcast::Sender<ConnectionState>>,
    last_heartbeat: Arc<Mutex<HashMap<NodeId, u64>>>, // Track last heartbeat timestamp per node
    data_stream_ready_tx: Arc<
        tokio::sync::RwLock<HashMap<NodeId, tokio::sync::oneshot::Sender<()>>>
    >,
}

impl MoqIrohEngine {
    /// Create a new engine instance
    pub async fn new(endpoint: Endpoint, gossip: Arc<Gossip>) -> Result<Self> {
        let (state_tx, _) = broadcast::channel(32);

        Ok(Self {
            endpoint: Arc::new(endpoint),
            gossip,
            stream_actors: Arc::new(DashMap::new()),
            connection_state: Arc::new(Mutex::new(ConnectionState::WaitingForPeer)),
            state_tx: Arc::new(state_tx),
            last_heartbeat: Arc::new(Mutex::new(HashMap::new())),
            data_stream_ready_tx: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        })
    }

    /// Helper function to normalize namespace for consistent lookups
    fn normalize_namespace(namespace: &str) -> String {
        let mut ns = namespace.to_string();

        // Remove duplicate slashes
        while ns.contains("//") {
            ns = ns.replace("//", "/");
        }

        // Ensure consistent handling of leading slashes
        if !ns.starts_with('/') {
            ns = format!("/{}", ns);
        }

        // Remove trailing slash if present
        if ns.len() > 1 && ns.ends_with('/') {
            ns = ns[..ns.len() - 1].to_string();
        }

        ns
    }

    /// Get a reference to the endpoint
    pub fn endpoint(&self) -> &Arc<Endpoint> {
        &self.endpoint
    }

    /// Get a reference to the gossip instance
    pub fn gossip(&self) -> &Arc<Gossip> {
        &self.gossip
    }

    /// Register a new stream and return a sender for publishing objects
    pub async fn register_stream(
        &self,
        stream_id: Uuid,
        namespace: String
    ) -> mpsc::Sender<MoqObject> {
        let normalized_ns = Self::normalize_namespace(&namespace);
        debug!("Normalizing namespace '{}' to '{}'", namespace, normalized_ns);

        let key = (stream_id, normalized_ns.clone());

        // Check if we already have a stream actor for this stream
        if let Some(cmd_tx) = self.stream_actors.get(&key) {
            info!(
                "Reusing existing stream actor for stream {} in namespace {}",
                stream_id,
                normalized_ns
            );

            // Create a channel that directly forwards to the existing stream actor
            let (tx, mut rx) = mpsc::channel::<MoqObject>(128);
            let cmd_tx_clone = cmd_tx.clone();

            tokio::spawn(async move {
                while let Some(object) = rx.recv().await {
                    if let Err(e) = cmd_tx_clone.send(StreamCommand::PublishObject(object)).await {
                        error!("Failed to send object to existing stream actor: {}", e);
                    }
                }
            });

            return tx;
        }

        // If no existing actor, create a new one
        let (cmd_tx, cmd_rx) = mpsc::channel::<StreamCommand>(128);

        // Start the stream actor
        tokio::spawn(Self::stream_actor(cmd_rx, stream_id, normalized_ns.clone()));

        // Store the command sender in our map
        self.stream_actors.insert(key, cmd_tx.clone());

        // Signal stream is ready
        self.update_connection_state(ConnectionState::Ready {
            node_id: self.endpoint.node_id(),
            stream_id,
            namespace: normalized_ns.clone(),
        }).await;

        debug!("Registered stream: {} in namespace: {}", stream_id, normalized_ns);

        // Create a channel that directly forwards to the stream actor
        // This simplifies the flow by avoiding the publish_object indirection
        let (tx, mut rx) = mpsc::channel::<MoqObject>(128);
        let cmd_tx_clone = cmd_tx.clone();

        tokio::spawn(async move {
            while let Some(object) = rx.recv().await {
                if let Err(e) = cmd_tx_clone.send(StreamCommand::PublishObject(object)).await {
                    error!("Failed to send object to stream actor: {}", e);
                }
            }
        });

        info!("Registered new stream: {} in namespace: {}", stream_id, normalized_ns);
        tx
    }

    /// Subscribe to a stream and return a receiver for objects
    pub async fn subscribe_to_stream(
        &self,
        publisher_id: NodeId,
        stream_id: Uuid,
        namespace: String,
        start_sequence: u64,
        group_id: u32,
        priority: u8
    ) -> Result<()> {
        let namespace = Self::normalize_namespace(&namespace);
        info!(
            "Subscribing to stream {} in namespace {} from publisher {}",
            stream_id,
            namespace,
            publisher_id
        );

        // Find or create stream actor
        let stream_key = (stream_id, namespace.clone());
        let available_actors: Vec<_> = self.stream_actors
            .iter()
            .map(|e| e.key().clone())
            .collect();
        debug!("Available stream actors: {:?}", available_actors);

        if let Some(tx) = self.stream_actors.get(&stream_key) {
            debug!("Found existing stream actor for key: {:?}", stream_key);
            // Send subscribe command to actor
            tx.send(StreamCommand::Subscribe {
                node_id: publisher_id,
                group_id,
            }).await.map_err(|e| anyhow::anyhow!("Failed to send subscribe command: {}", e));
        } else {
            debug!("Stream actor not found for key: {:?}", stream_key);
            // Create new stream actor for remote stream
            let (cmd_tx, cmd_rx) = mpsc::channel(512);
            tokio::spawn(Self::stream_actor(cmd_rx, stream_id, namespace.clone()));

            // Register stream actor
            self.stream_actors.insert(stream_key, cmd_tx.clone());

            // Send subscribe command to actor
            cmd_tx
                .send(StreamCommand::Subscribe {
                    node_id: publisher_id,
                    group_id,
                }).await
                .map_err(|e| anyhow::anyhow!("Failed to send subscribe command: {}", e));
        }

        debug!("Subscription registered for stream: {} in namespace: {}", stream_id, namespace);

        Ok(()) // No longer returning a receiver
    }

    /// Set the data stream for a subscriber
    pub async fn set_data_stream(
        &self,
        stream_id: Uuid,
        namespace: String,
        node_id: NodeId,
        stream: SendStream
    ) -> Result<()> {
        let normalized_ns = Self::normalize_namespace(&namespace);
        debug!("Normalizing namespace '{}' to '{}'", namespace, normalized_ns);

        debug!(
            "Setting data stream for subscriber {} in stream {} namespace {}",
            node_id,
            stream_id,
            normalized_ns
        );

        // Debug log all stream actors for troubleshooting
        let keys: Vec<_> = self.stream_actors
            .iter()
            .map(|entry| format!("({}, {})", entry.key().0, entry.key().1))
            .collect();
        debug!("Available stream actors when setting data stream: {:?}", keys);

        if let Some(cmd_tx) = self.stream_actors.get(&(stream_id, normalized_ns.clone())) {
            if
                let Err(e) = cmd_tx.send(StreamCommand::SetDataStream {
                    node_id,
                    stream,
                }).await
            {
                error!("Failed to send set data stream command: {}", e);
                return bail!("Stream actor channel closed");
            }
        } else {
            error!(
                "Stream not found: {} namespace '{}' (normalized from '{}')",
                stream_id,
                normalized_ns,
                namespace
            );
            return bail!("Stream not found");
        }

        Ok(())
    }

    /// Handle retransmission requests
    pub async fn request_retransmission(
        &self,
        stream_id: Uuid,
        namespace: String,
        node_id: NodeId,
        sequence: u64
    ) -> Result<()> {
        let normalized_ns = Self::normalize_namespace(&namespace);
        debug!("Normalizing namespace '{}' to '{}'", namespace, normalized_ns);

        debug!(
            "Requesting retransmission of sequence {} in stream {} for {}",
            sequence,
            stream_id,
            node_id
        );

        if let Some(cmd_tx) = self.stream_actors.get(&(stream_id, normalized_ns.clone())) {
            if
                let Err(e) = cmd_tx.send(StreamCommand::RequestRetransmission {
                    node_id,
                    sequence,
                }).await
            {
                error!("Failed to send retransmission request: {}", e);
                return bail!("Stream actor channel closed");
            }
        } else {
            error!("Stream not found: {}/{}", stream_id, normalized_ns);
            return bail!("Stream not found");
        }

        Ok(())
    }

    /// Subscribe to connection state changes
    pub fn subscribe_to_state_changes(&self) -> broadcast::Receiver<ConnectionState> {
        self.state_tx.subscribe()
    }

    /// Update connection state and notify subscribers only if state actually changes
    pub async fn update_connection_state(&self, state: ConnectionState) {
        let mut current = self.connection_state.lock().await;
        if *current != state {
            *current = state.clone();
            info!("Connection state updated: {:?}", state);
            if let Err(_) = self.state_tx.send(state) {
                debug!("No active subscribers for connection state updates");
            }
        }
    }

    /// Signal a connection event and update state accordingly
    pub async fn signal_connection_event(&self, event: ConnectionEvent) {
        match event {
            ConnectionEvent::PeerConnected(node_id) => {
                info!("Peer connected: {}", node_id);
                self.update_connection_state(ConnectionState::Connected(node_id)).await;
            }
            ConnectionEvent::StreamReady { node_id, stream_id, namespace } => {
                info!("Stream ready: {} in namespace {}", stream_id, namespace);
                self.update_connection_state(ConnectionState::Ready {
                    node_id,
                    stream_id,
                    namespace,
                }).await;
            }
        }
    }

    /// Send subscribe_ok notification to a node
    pub async fn notify_subscribe_ok(&self, node_id: NodeId) -> Result<()> {
        let mut map = self.data_stream_ready_tx.write().await;
        if let Some(tx) = map.remove(&node_id) {
            if let Err(_) = tx.send(()) {
                warn!("Failed to send subscribe_ok notification - receiver dropped");
            }
        }
        Ok(())
    }

    /// Register a subscribe_ok notification channel
    pub async fn register_subscribe_ok_channel(
        &self,
        node_id: NodeId,
        tx: tokio::sync::oneshot::Sender<()>
    ) {
        let mut map = self.data_stream_ready_tx.write().await;
        map.insert(node_id, tx);
    }

    /// Unsubscribe from a stream
    pub async fn unsubscribe(
        &self,
        stream_id: Uuid,
        namespace: String,
        node_id: NodeId
    ) -> Result<()> {
        let normalized_ns = Self::normalize_namespace(&namespace);
        debug!("Normalizing namespace '{}' to '{}'", namespace, normalized_ns);

        debug!(
            "Unsubscribing node {} from stream {} namespace {}",
            node_id,
            stream_id,
            normalized_ns
        );

        if let Some(cmd_tx) = self.stream_actors.get(&(stream_id, normalized_ns.clone())) {
            if let Err(e) = cmd_tx.send(StreamCommand::Unsubscribe(node_id)).await {
                error!("Failed to send unsubscribe command: {}", e);
                return bail!("Stream actor channel closed");
            }
        } else {
            // No-op if stream doesn't exist
            debug!("Stream not found for unsubscribe: {}/{}", stream_id, normalized_ns);
        }

        Ok(())
    }

    /// The stream actor function that processes commands for a single stream
    async fn stream_actor(
        mut cmd_rx: mpsc::Receiver<StreamCommand>,
        stream_id: Uuid,
        namespace: String
    ) {
        info!("Starting stream actor for {} in namespace {}", stream_id, namespace);

        // State for this stream
        let mut subscribers: HashMap<NodeId, (u32, Option<SendStream>)> = HashMap::new();
        let mut buffer: VecDeque<MoqObject> = VecDeque::with_capacity(SLIDING_WINDOW_SIZE);
        let mut init_segment: Option<MoqObject> = None;

        // Process commands until the channel is closed
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                StreamCommand::PublishObject(object) => {
                    // Store init segment if needed
                    if object.group_id == (MEDIA_TYPE_INIT as u32) {
                        debug!("Storing init segment for stream {}", stream_id);
                        init_segment = Some(object.clone());
                    }

                    // Add to buffer, maintaining size limit
                    buffer.push_back(object.clone());
                    if buffer.len() > SLIDING_WINDOW_SIZE {
                        buffer.pop_front();
                    }

                    // Send to all matching subscribers
                    for (node_id, (group_id, data_stream_opt)) in &mut subscribers {
                        let is_matching = *group_id == 0 || *group_id == object.group_id;

                        if is_matching {
                            // Try sending via data stream if available
                            if let Some(data_stream) = data_stream_opt.as_mut() {
                                if
                                    let Err(e) = Self::send_object_to_stream(
                                        data_stream,
                                        &object
                                    ).await
                                {
                                    error!(
                                        "Failed to send object to subscriber {}: {}",
                                        node_id,
                                        e
                                    );
                                }
                            } else {
                                debug!("No data stream available for subscriber {}", node_id);
                            }
                        }
                    }
                }
                StreamCommand::Subscribe { node_id, group_id } => {
                    debug!(
                        "Adding subscriber {} with group_id {} to stream {}",
                        node_id,
                        group_id,
                        stream_id
                    );
                    subscribers.insert(node_id, (group_id, None));
                }
                StreamCommand::SetDataStream { node_id, stream } => {
                    debug!(
                        "Setting data stream for subscriber {} in stream {}",
                        node_id,
                        stream_id
                    );

                    let subscriber_found = subscribers.contains_key(&node_id);
                    info!(
                        "SetDataStream for node_id={}, subscriber found={}",
                        node_id,
                        subscriber_found
                    );

                    if let Some((group_id, data_stream_opt)) = subscribers.get_mut(&node_id) {
                        // Set the data stream
                        *data_stream_opt = Some(stream);
                        info!(
                            "Data stream set for subscriber {} with group_id={} on stream {}",
                            node_id,
                            group_id,
                            stream_id
                        );

                        // Send init segment if available and applicable
                        if let Some(init) = &init_segment {
                            let applies = *group_id == 0 || *group_id == init.group_id;
                            info!(
                                "Init segment status for subscriber {}: available=true, applies={}, group_id={}, init.group_id={}",
                                node_id,
                                applies,
                                group_id,
                                init.group_id
                            );

                            if applies {
                                if let Some(data_stream) = data_stream_opt.as_mut() {
                                    debug!("Sending init segment to new subscriber {}", node_id);
                                    if
                                        let Err(e) = Self::send_object_to_stream(
                                            data_stream,
                                            init
                                        ).await
                                    {
                                        error!(
                                            "Failed to send init segment to subscriber {}: {}",
                                            node_id,
                                            e
                                        );
                                    } else {
                                        info!("Successfully sent init segment to subscriber {}", node_id);
                                    }
                                }
                            }
                        } else {
                            info!(
                                "No init segment available yet for subscriber {} on stream {}",
                                node_id,
                                stream_id
                            );
                        }

                        // Send buffered objects to the new subscriber
                        if let Some(data_stream) = data_stream_opt.as_mut() {
                            for obj in buffer.iter() {
                                if *group_id == 0 || *group_id == obj.group_id {
                                    if
                                        let Err(e) = Self::send_object_to_stream(
                                            data_stream,
                                            obj
                                        ).await
                                    {
                                        error!(
                                            "Failed to send buffered object to subscriber {}: {}",
                                            node_id,
                                            e
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                    } else {
                        error!("Subscriber {} not found when setting data stream", node_id);
                    }
                }
                StreamCommand::Unsubscribe(node_id) => {
                    debug!("Removing subscriber {} from stream {}", node_id, stream_id);
                    subscribers.remove(&node_id);
                }
                StreamCommand::RequestRetransmission { node_id, sequence } => {
                    debug!(
                        "Processing retransmission request for sequence {} from {}",
                        sequence,
                        node_id
                    );

                    if let Some(obj) = buffer.iter().find(|o| o.sequence == sequence) {
                        if let Some((group_id, data_stream_opt)) = subscribers.get_mut(&node_id) {
                            if *group_id == 0 || *group_id == obj.group_id {
                                if let Some(data_stream) = data_stream_opt.as_mut() {
                                    debug!("Retransmitting object with sequence {}", sequence);
                                    if
                                        let Err(e) = Self::send_object_to_stream(
                                            data_stream,
                                            obj
                                        ).await
                                    {
                                        error!("Failed to retransmit object to {}: {}", node_id, e);
                                    }
                                }
                            }
                        }
                    } else {
                        debug!("Object with sequence {} not found for retransmission", sequence);
                    }
                }
            }
        }

        info!("Stream actor for {} in namespace {} terminated", stream_id, namespace);
    }

    /// Sends a serialized MoqObject over a SendStream with error handling and metrics.
    ///
    /// This function serializes a MoqObject into a binary format and sends it over
    /// the provided SendStream. It includes proper error handling, timeouts, and metrics logging.
    ///
    /// # Arguments
    /// * `stream` - A mutable reference to the SendStream to send the object over
    /// * `object` - A reference to the MoqObject to send
    ///
    /// # Returns
    /// * `Ok(())` if the object was successfully sent
    /// * `Err` with a detailed error message if sending failed
    async fn send_object_to_stream(stream: &mut SendStream, object: &MoqObject) -> Result<()> {
        // Start timing for metrics
        let start = std::time::Instant::now();

        // Create a buffer with the serialized object
        let mut serialized_data = Vec::new();

        // Simple serialization format:
        // - Object name length (u16) + Object name bytes
        // - Sequence number (u64)
        // - Timestamp (u64)
        // - Group ID (u32)
        // - Priority (u8)
        // - Data length (u32) + Data bytes

        // Object name (length prefixed)
        let name_bytes = object.name.as_bytes();
        let name_len = name_bytes.len();
        if name_len > 65535 {
            error!("Object name too long: {} bytes", name_len);
            bail!("Object name exceeds maximum length");
        }

        serialized_data.extend_from_slice(&(name_len as u16).to_be_bytes());
        serialized_data.extend_from_slice(name_bytes);

        // Sequence, timestamp, group, priority
        serialized_data.extend_from_slice(&object.sequence.to_be_bytes());
        serialized_data.extend_from_slice(&object.timestamp.to_be_bytes());
        serialized_data.extend_from_slice(&object.group_id.to_be_bytes());
        serialized_data.push(object.priority);

        // Data (length prefixed)
        let data_len = object.data.len();
        serialized_data.extend_from_slice(&(data_len as u32).to_be_bytes());
        serialized_data.extend_from_slice(&object.data);

        // Add total length prefix
        let total_len = serialized_data.len();
        let mut final_data = Vec::with_capacity(4 + total_len);
        final_data.extend_from_slice(&(total_len as u32).to_be_bytes());
        final_data.extend(serialized_data);

        // Send the serialized data over the stream with timeout

        // Use a timeout to prevent hanging on network issues
        match
            tokio::time::timeout(
                std::time::Duration::from_secs(5), // 5 second timeout
                stream.write_all(&final_data)
            ).await
        {
            Ok(Ok(_)) => {
                // Successfully wrote data, now flush with timeout
                match
                    tokio::time::timeout(
                        std::time::Duration::from_secs(2), // 2 second timeout for flush
                        stream.flush()
                    ).await
                {
                    Ok(Ok(_)) => {
                        let duration = start.elapsed();
                        trace!(
                            "Successfully sent object with sequence {} (size: {} bytes) in {:?}",
                            object.sequence,
                            final_data.len(),
                            duration
                        );
                        Ok(())
                    }
                    Ok(Err(e)) => {
                        error!(
                            "Failed to flush stream after sending object {}: {}",
                            object.sequence,
                            e
                        );
                        Err(e.into())
                    }
                    Err(_) => {
                        error!("Timeout while flushing stream for object {}", object.sequence);
                        Err(anyhow::anyhow!("Flush operation timed out"))
                    }
                }
            }
            Ok(Err(e)) => {
                error!("Failed to send object with sequence {}: {}", object.sequence, e);
                Err(e.into())
            }
            Err(_) => {
                error!("Timeout while sending object with sequence {}", object.sequence);
                Err(anyhow::anyhow!("Write operation timed out"))
            }
        }
    }
}
