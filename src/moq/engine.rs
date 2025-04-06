use crate::moq::proto::{ MoqObject, MEDIA_TYPE_INIT };
use iroh::Endpoint;
use iroh::endpoint::SendStream;
use iroh::NodeId;
use iroh_gossip::net::Gossip;
use anyhow::{ Result, bail };
use tokio::sync::{ mpsc, Mutex, broadcast };
use std::sync::Arc;
use tracing::{ info, error, debug, warn, trace };
use std::collections::{ HashMap, VecDeque };
use dashmap::DashMap;
use uuid::Uuid;
use tokio::io::AsyncWriteExt;
use std::time::Duration;

// Constants
const SLIDING_WINDOW_SIZE: usize = 100; // Max objects in retransmission buffer
const DEFAULT_COMMAND_CHANNEL_SIZE: usize = 128;
const SUBSCRIBE_COMMAND_CHANNEL_SIZE: usize = 512; // Potentially larger for subscribe bursts
const OBJECT_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const OBJECT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

// State associated with a single stream actor
#[derive(Debug)]
struct StreamActorState {
    subscribers: HashMap<NodeId, (u32, Option<SendStream>)>,
    buffer: VecDeque<MoqObject>,
    init_segment: Option<MoqObject>,
    stream_id: Uuid,
    namespace: String,
}

impl StreamActorState {
    fn new(stream_id: Uuid, namespace: String) -> Self {
        Self {
            subscribers: HashMap::new(),
            buffer: VecDeque::with_capacity(SLIDING_WINDOW_SIZE),
            init_segment: None,
            stream_id,
            namespace,
        }
    }

    // Helper to add object to buffer, handling init segment and size limit
    fn add_object_to_buffer(&mut self, object: MoqObject) {
        if object.group_id == (MEDIA_TYPE_INIT as u32) {
            debug!("Storing init segment for stream {}", self.stream_id);
            self.init_segment = Some(object.clone());
        }
        self.buffer.push_back(object);
        if self.buffer.len() > SLIDING_WINDOW_SIZE {
            self.buffer.pop_front();
        }
    }
}

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
    _last_heartbeat: Arc<Mutex<HashMap<NodeId, u64>>>, // Track last heartbeat timestamp per node
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
            _last_heartbeat: Arc::new(Mutex::new(HashMap::new())),
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
        let (cmd_tx, cmd_rx) = mpsc::channel::<StreamCommand>(DEFAULT_COMMAND_CHANNEL_SIZE);

        // Start the stream actor
        tokio::spawn(Self::stream_actor_loop(cmd_rx, stream_id, normalized_ns.clone()));

        // Store the command sender in our map
        self.stream_actors.insert(key, cmd_tx.clone());

        // Signal stream is ready
        self.update_connection_state(ConnectionState::Ready {
            node_id: self.endpoint.node_id(),
            stream_id,
            namespace: normalized_ns.clone(),
        }).await;

        // Create a channel that directly forwards to the stream actor
        // This simplifies the flow by avoiding the publish_object indirection
        let (tx, mut rx) = mpsc::channel::<MoqObject>(DEFAULT_COMMAND_CHANNEL_SIZE);
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
        _start_sequence: u64,
        group_id: u32,
        _priority: u8
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

        if let Some(tx) = self.stream_actors.get(&stream_key) {
            // Send subscribe command to actor
            let _ = tx
                .send(StreamCommand::Subscribe {
                    node_id: publisher_id,
                    group_id,
                }).await
                .map_err(|e| anyhow::anyhow!("Failed to send subscribe command: {}", e));
        } else {
            // Create new stream actor for remote stream
            let (cmd_tx, cmd_rx) = mpsc::channel(SUBSCRIBE_COMMAND_CHANNEL_SIZE);
            tokio::spawn(Self::stream_actor_loop(cmd_rx, stream_id, namespace.clone()));

            // Register stream actor
            self.stream_actors.insert(stream_key, cmd_tx.clone());

            // Send subscribe command to actor
            let _result = cmd_tx
                .send(StreamCommand::Subscribe {
                    node_id: publisher_id,
                    group_id,
                }).await
                .map_err(|e| anyhow::anyhow!("Failed to send subscribe command: {}", e));
        }

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

        debug!(
            "Setting data stream for subscriber {} in stream {} namespace {}",
            node_id,
            stream_id,
            normalized_ns
        );

        if let Some(cmd_tx) = self.stream_actors.get(&(stream_id, normalized_ns.clone())) {
            if
                let Err(e) = cmd_tx.send(StreamCommand::SetDataStream {
                    node_id,
                    stream,
                }).await
            {
                error!("Failed to send set data stream command: {}", e);
            }
        } else {
            error!(
                "Stream not found: {} namespace '{}' (normalized from '{}')",
                stream_id,
                normalized_ns,
                namespace
            );
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
            }
        } else {
            error!("Stream not found: {}/{}", stream_id, normalized_ns);
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

        debug!(
            "Unsubscribing node {} from stream {} namespace {}",
            node_id,
            stream_id,
            normalized_ns
        );

        if let Some(cmd_tx) = self.stream_actors.get(&(stream_id, normalized_ns.clone())) {
            if let Err(e) = cmd_tx.send(StreamCommand::Unsubscribe(node_id)).await {
                error!("Failed to send unsubscribe command: {}", e);
            }
        } else {
            // No-op if stream doesn't exist
            debug!("Stream not found for unsubscribe: {}/{}", stream_id, normalized_ns);
        }

        Ok(())
    }

    /// The main loop for a stream actor, processing commands.
    async fn stream_actor_loop(
        mut cmd_rx: mpsc::Receiver<StreamCommand>,
        stream_id: Uuid,
        namespace: String
    ) {
        info!("Starting stream actor for {} in namespace {}", stream_id, namespace);

        let mut state = StreamActorState::new(stream_id, namespace.clone());

        // Process commands until the channel is closed
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                StreamCommand::PublishObject(object) => {
                    Self::handle_publish_object(&mut state, object).await;
                }
                StreamCommand::Subscribe { node_id, group_id } => {
                    Self::handle_subscribe(&mut state, node_id, group_id);
                }
                StreamCommand::SetDataStream { node_id, stream } => {
                    Self::handle_set_data_stream(&mut state, node_id, stream).await;
                }
                StreamCommand::Unsubscribe(node_id) => {
                    Self::handle_unsubscribe(&mut state, node_id);
                }
                StreamCommand::RequestRetransmission { node_id, sequence } => {
                    Self::handle_retransmission_request(&mut state, node_id, sequence).await;
                }
            }
        }

        info!("Stream actor for {} in namespace {} terminated", stream_id, namespace);
    }

    /// Handles the PublishObject command for a stream actor.
    async fn handle_publish_object(state: &mut StreamActorState, object: MoqObject) {
        state.add_object_to_buffer(object.clone());

        // Send to all matching subscribers
        for (node_id, (group_id, data_stream_opt)) in &mut state.subscribers {
            let is_matching = *group_id == 0 || *group_id == object.group_id;

            if is_matching {
                if let Some(data_stream) = data_stream_opt.as_mut() {
                    if let Err(e) = Self::send_object_to_stream(data_stream, &object).await {
                        error!(
                            "Failed to send object seq {} to subscriber {}: {}",
                            object.sequence,
                            node_id,
                            e
                        );
                        // Potential actions: Mark subscriber as unhealthy? Remove? For now, just log.
                    }
                } else {
                    // This case might happen briefly between Subscribe and SetDataStream
                    // Or if SetDataStream fails. Logging it might be too noisy.
                    // debug!("No data stream available yet for subscriber {}", node_id);
                }
            }
        }
    }

    /// Handles the Subscribe command for a stream actor.
    fn handle_subscribe(state: &mut StreamActorState, node_id: NodeId, group_id: u32) {
        debug!(
            "Adding subscriber {} with group_id {} to stream {}",
            node_id,
            group_id,
            state.stream_id
        );
        // Insert/update subscriber without a data stream initially.
        state.subscribers.insert(node_id, (group_id, None));
    }

    /// Handles the SetDataStream command for a stream actor.
    async fn handle_set_data_stream(
        state: &mut StreamActorState,
        node_id: NodeId,
        mut stream: SendStream // Take ownership
    ) {
        debug!("Setting data stream for subscriber {} in stream {}", node_id, state.stream_id);

        if let Some((group_id, data_stream_opt)) = state.subscribers.get_mut(&node_id) {
            info!(
                "Data stream established for subscriber {} (group_id {}) on stream {}",
                node_id,
                group_id,
                state.stream_id
            );

            // Send init segment if available and applicable
            if let Some(init) = &state.init_segment {
                let applies = *group_id == 0 || *group_id == init.group_id;
                if applies {
                    debug!("Sending init segment to new subscriber {}", node_id);
                    if let Err(e) = Self::send_object_to_stream(&mut stream, init).await {
                        error!("Failed to send init segment to subscriber {}: {}", node_id, e);
                        // If we fail to send the init segment, the subscriber might be broken.
                        // We could remove the subscriber here, but let's keep it simple for now.
                    } else {
                        info!("Successfully sent init segment to subscriber {}", node_id);
                    }
                }
            }

            // Send buffered objects to the new subscriber
            for obj in state.buffer.iter() {
                // Check group ID match again for safety
                if *group_id == 0 || *group_id == obj.group_id {
                    if let Err(e) = Self::send_object_to_stream(&mut stream, obj).await {
                        error!(
                            "Failed to send buffered object seq {} to subscriber {}: {}",
                            obj.sequence,
                            node_id,
                            e
                        );
                        // If sending buffer fails, stop sending more to this subscriber for now.
                        break;
                    }
                }
            }

            // Store the stream only after successfully sending initial data
            *data_stream_opt = Some(stream);
        } else {
            // This could happen if Unsubscribe is processed before SetDataStream arrives.
            warn!(
                "Subscriber {} not found when setting data stream for stream {}",
                node_id,
                state.stream_id
            );
            // Close the provided stream as it won't be used.
            if let Err(e) = stream.finish() {
                warn!("Failed to finish unused stream for {}: {}", node_id, e);
            }
        }
    }

    /// Handles the Unsubscribe command for a stream actor.
    fn handle_unsubscribe(state: &mut StreamActorState, node_id: NodeId) {
        debug!("Removing subscriber {} from stream {}", node_id, state.stream_id);
        if let Some((_group_id, stream_opt)) = state.subscribers.remove(&node_id) {
            // TODO: Should we try to close the SendStream here if it exists?
            // `stream.finish()` might be needed, but requires async context.
            // For now, we rely on the SendStream being dropped.
            if stream_opt.is_some() {
                debug!("Removed subscriber {} had an active data stream.", node_id);
            }
        } else {
            debug!("Unsubscribe request for non-existent subscriber {}", node_id);
        }
    }

    /// Handles the RequestRetransmission command for a stream actor.
    async fn handle_retransmission_request(
        state: &mut StreamActorState,
        node_id: NodeId,
        sequence: u64
    ) {
        debug!(
            "Processing retransmission request for seq {} from {} on stream {}",
            sequence,
            node_id,
            state.stream_id
        );

        if let Some((group_id, data_stream_opt)) = state.subscribers.get_mut(&node_id) {
            if let Some(data_stream) = data_stream_opt.as_mut() {
                // Find the object in the buffer
                if let Some(obj) = state.buffer.iter().find(|o| o.sequence == sequence) {
                    // Check if the group matches before sending
                    if *group_id == 0 || *group_id == obj.group_id {
                        debug!("Retransmitting object with sequence {} to {}", sequence, node_id);
                        if let Err(e) = Self::send_object_to_stream(data_stream, obj).await {
                            error!(
                                "Failed to retransmit object seq {} to {}: {}",
                                sequence,
                                node_id,
                                e
                            );
                        }
                    } else {
                        debug!(
                            "Object seq {} found but group mismatch (sub: {}, obj: {}) for {}",
                            sequence,
                            group_id,
                            obj.group_id,
                            node_id
                        );
                    }
                } else {
                    debug!("Object with sequence {} not found in buffer for retransmission", sequence);
                }
            } else {
                warn!("Cannot retransmit seq {} to {}: Data stream not set", sequence, node_id);
            }
        } else {
            warn!("Cannot retransmit seq {}: Subscriber {} not found", sequence, node_id);
        }
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
        let start = std::time::Instant::now();

        let serialized_data = match Self::serialize_object(object) {
            Ok(data) => data,
            Err(e) => {
                error!("Serialization error for object seq {}: {}", object.sequence, e);
                return Err(e);
            }
        };

        // Send the serialized data over the stream with timeout
        match
            tokio::time::timeout(OBJECT_SEND_TIMEOUT, stream.write_all(&serialized_data)).await // Use constant
        {
            Ok(Ok(_)) => {
                // Successfully wrote data, now flush with timeout
                match
                    tokio::time::timeout(OBJECT_FLUSH_TIMEOUT, stream.flush()).await // Use constant
                {
                    Ok(Ok(_)) => {
                        let duration = start.elapsed();
                        // Trace log is fine for performance debugging if needed via feature flag/level
                        trace!(
                            "Successfully sent object with sequence {} (size: {} bytes) in {:?}",
                            object.sequence,
                            serialized_data.len(),
                            duration
                        );
                        Ok(())
                    }
                    Ok(Err(e)) => {
                        // Handle flush error when timeout doesn't occur
                        error!(
                            "Failed to flush stream after sending object seq {}: {}",
                            object.sequence,
                            e
                        );
                        Err(e.into())
                    }
                    Err(_) => {
                        error!("Timeout while flushing stream for object seq {}", object.sequence);
                        Err(
                            anyhow::anyhow!(
                                "Flush operation timed out after {:?}",
                                OBJECT_FLUSH_TIMEOUT
                            )
                        )
                    }
                }
            }
            Ok(Err(e)) => {
                error!("Failed to send object with sequence {}: {}", object.sequence, e);
                Err(e.into())
            }
            Err(_) => {
                error!("Timeout while sending object with sequence {}", object.sequence);
                Err(anyhow::anyhow!("Write operation timed out after {:?}", OBJECT_SEND_TIMEOUT))
            }
        }
    }

    /// Serializes a MoqObject into a byte vector with length prefixing.
    fn serialize_object(object: &MoqObject) -> Result<Vec<u8>> {
        let mut buffer = Vec::new();

        // Simple serialization format:
        // - Total length (u32) [Added at the end]
        // - Object name length (u16) + Object name bytes
        // - Sequence number (u64)
        // - Timestamp (u64)
        // - Group ID (u32)
        // - Priority (u8)
        // - Data length (u32) + Data bytes

        // Object name (length prefixed)
        let name_bytes = object.name.as_bytes();
        let name_len = name_bytes.len();
        if name_len > (u16::MAX as usize) {
            bail!("Object name too long: {} bytes (max {})", name_len, u16::MAX);
        }
        buffer.extend_from_slice(&(name_len as u16).to_be_bytes());
        buffer.extend_from_slice(name_bytes);

        // Sequence, timestamp, group, priority
        buffer.extend_from_slice(&object.sequence.to_be_bytes());
        buffer.extend_from_slice(&object.timestamp.to_be_bytes());
        buffer.extend_from_slice(&object.group_id.to_be_bytes());
        buffer.push(object.priority);

        // Data (length prefixed)
        let data_len = object.data.len();
        if data_len > (u32::MAX as usize) {
            // This is highly unlikely with typical network MTUs, but good practice to check.
            bail!("Object data too large: {} bytes (max {})", data_len, u32::MAX);
        }
        buffer.extend_from_slice(&(data_len as u32).to_be_bytes());
        buffer.extend_from_slice(&object.data);

        // Prepend total length
        let total_inner_len = buffer.len();
        if total_inner_len > (u32::MAX as usize) {
            bail!("Total serialized object size exceeds limit: {} bytes", total_inner_len);
        }
        let mut final_data = Vec::with_capacity(4 + total_inner_len);
        final_data.extend_from_slice(&(total_inner_len as u32).to_be_bytes());
        final_data.extend(buffer);

        Ok(final_data)
    }
}
