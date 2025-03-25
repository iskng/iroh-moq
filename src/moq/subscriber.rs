use crate::moq::proto::{
    MoqObject,
    MediaInit,
    VideoChunk,
    ALPN,
    StreamAnnouncement,
    serialize_subscribe,
    TYPE_SUBSCRIBE_OK,
    MEDIA_TYPE_INIT,
    MEDIA_TYPE_VIDEO,
};
use crate::moq::engine::MoqIrohEngine;
use anyhow::{ Result, bail, anyhow };
use iroh::endpoint::{ Connection, SendStream, RecvStream };
use iroh::{ Endpoint, NodeId };
use tokio::sync::mpsc;
use std::sync::Arc;
use tracing::{ info, error, debug, trace };
use uuid::Uuid;
use bytes::{ BytesMut, Buf };
use tokio::io::AsyncWriteExt;
use std::io::Cursor;
use std::io::Read;

// Protocol-specific constants
const STREAM_TYPE_CONTROL: u8 = 0x01;
const STREAM_TYPE_DATA: u8 = 0x02;

// SubscriberState enum tracking the actor's current state
#[derive(Debug, Clone, PartialEq)]
enum SubscriberState {
    // Initial state before any connection is made
    Initial,
    // Connected to the publisher but no streams established
    Connecting,
    // Control stream established with publisher
    ControlStreamEstablished,
    // Subscribe message sent, waiting for confirmation
    Subscribing,
    // Fully subscribed with data stream established
    Subscribed,
    // Error state
    Error(String),
    // Terminal state
    Terminated(String), // The error is stored as a string to make the enum Clone+PartialEq
}

// Commands that drive the state machine
#[derive(Debug)]
pub enum SubscriberCommand {
    Connect,
    OpenControlStream,
    Subscribe,
    OpenDataStream,
    SubscribeOk,
    ProcessObject(MoqObject),
    RequestRetransmission(u64),
    Terminate(anyhow::Error),
}

// SubscriberActor holds all the state for the subscription
struct SubscriberActor {
    // Current state of the actor
    state: SubscriberState,
    // Publisher's node ID
    publisher_id: NodeId,
    // Stream ID we're subscribing to
    stream_id: Uuid,
    // Stream namespace
    namespace: String,
    // Original announcement
    announcement: StreamAnnouncement,
    // Active connection to the publisher
    connection: Option<Connection>,
    // Control stream
    control_stream: Option<(SendStream, RecvStream)>,
    // Control stream send only
    control_stream_send: Option<SendStream>,
    // Data stream - once established
    data_stream: Option<(SendStream, RecvStream)>,
    // Media initialization output channel
    init_tx: mpsc::Sender<MediaInit>,
    // Video chunks output channel
    chunk_tx: mpsc::Sender<VideoChunk>,
    // Command channel - for sending commands to self
    cmd_tx: mpsc::Sender<SubscriberCommand>,
    // Command receiver
    cmd_rx: mpsc::Receiver<SubscriberCommand>,
    // Engine reference
    engine: Arc<MoqIrohEngine>,
    // Endpoint for establishing connections
    endpoint: Arc<Endpoint>,
}

impl SubscriberActor {
    // Create a new subscriber actor and start its state machine
    async fn new(
        endpoint: Arc<Endpoint>,
        engine: Arc<MoqIrohEngine>,
        announcement: StreamAnnouncement
    ) -> Result<(mpsc::Receiver<MediaInit>, mpsc::Receiver<VideoChunk>)> {
        info!(
            "Creating subscriber actor for stream {} from {}",
            announcement.stream_id,
            announcement.sender_id
        );

        // Create channels for output
        let (init_tx, init_rx) = mpsc::channel::<MediaInit>(10);
        let (chunk_tx, chunk_rx) = mpsc::channel::<VideoChunk>(1024);

        // Create command channel
        let (cmd_tx, cmd_rx) = mpsc::channel::<SubscriberCommand>(100);

        // Create the actor
        let mut actor = SubscriberActor {
            state: SubscriberState::Initial,
            publisher_id: announcement.sender_id,
            stream_id: announcement.stream_id,
            namespace: announcement.namespace.clone(),
            announcement: announcement.clone(),
            connection: None,
            control_stream: None,
            data_stream: None,
            init_tx,
            chunk_tx,
            cmd_tx: cmd_tx.clone(),
            cmd_rx,
            engine,
            endpoint,
            control_stream_send: None,
        };

        // Start the actor's state machine in a separate task
        tokio::spawn(async move {
            actor.run().await;
        });

        // Send initial command to start connecting
        if let Err(e) = cmd_tx.send(SubscriberCommand::Connect).await {
            bail!("Failed to start subscriber actor: {}", e);
        }

        // Return receivers for client to listen on
        Ok((init_rx, chunk_rx))
    }

    async fn run(&mut self) {
        debug!(
            "Starting subscriber actor for stream {} from {}",
            self.stream_id,
            self.publisher_id
        );

        // Main actor loop
        while let Some(cmd) = self.cmd_rx.recv().await {
            // Always process Terminate immediately
            if let SubscriberCommand::Terminate(err) = cmd {
                self.handle_terminate(err).await;
                break;
            }

            // Process commands based on current state
            match self.state {
                SubscriberState::Initial => {
                    match cmd {
                        SubscriberCommand::Connect => {
                            info!("handling connect for subscriber");
                            self.handle_connect().await;
                        }
                        _ => {
                            error!("Invalid command {:?} for state {:?}", cmd, self.state);
                        }
                    }
                }
                SubscriberState::Connecting => {
                    match cmd {
                        SubscriberCommand::OpenControlStream => {
                            self.handle_open_control_stream().await;
                        }
                        _ => {
                            error!("Invalid command {:?} for state {:?}", cmd, self.state);
                        }
                    }
                }
                SubscriberState::ControlStreamEstablished => {
                    match cmd {
                        SubscriberCommand::Subscribe => self.handle_subscribe().await,
                        _ => {
                            error!("Invalid command {:?} for state {:?}", cmd, self.state);
                        }
                    }
                }
                SubscriberState::Subscribing => {
                    match cmd {
                        SubscriberCommand::SubscribeOk => {
                            self.handle_subscribe_ok().await;
                        }
                        _ => {
                            error!("Invalid command {:?} for state {:?}", cmd, self.state);
                        }
                    }
                }
                SubscriberState::Subscribed => {
                    match cmd {
                        SubscriberCommand::OpenDataStream => {
                            self.handle_open_data_stream().await;
                        }
                        SubscriberCommand::ProcessObject(obj) => {
                            self.handle_process_object(obj).await;
                        }
                        _ => {
                            error!("Invalid command {:?} for state {:?}", cmd, self.state);
                        }
                    }
                }
                SubscriberState::Error(_) | SubscriberState::Terminated(_) => {
                    // No processing in terminal states
                    debug!("Command {:?} ignored in terminal state", cmd);
                }
            }
        }

        debug!("Subscriber actor terminated for stream {}", self.stream_id);
    }

    async fn handle_connect(&mut self) {
        info!("Connecting to publisher {} for stream {}", self.publisher_id, self.stream_id);

        self.state = SubscriberState::Connecting;

        // Connect to the publisher
        match self.endpoint.connect(self.publisher_id, ALPN).await {
            Ok(conn) => {
                debug!(
                    "Successfully connected to publisher {} for stream {}",
                    self.publisher_id,
                    self.stream_id
                );
                self.connection = Some(conn);

                // Transition to next state
                if let Err(e) = self.cmd_tx.send(SubscriberCommand::OpenControlStream).await {
                    error!("Failed to send OpenControlStream command: {}", e);
                    self.handle_terminate(anyhow!("Command channel error: {}", e)).await;
                }
            }
            Err(e) => {
                error!("Failed to connect to publisher {}: {}", self.publisher_id, e);
                self.handle_terminate(anyhow!("Connection error: {}", e)).await;
            }
        }
    }

    async fn handle_open_control_stream(&mut self) {
        info!(
            "Opening control stream to publisher {} for stream {}",
            self.publisher_id,
            self.stream_id
        );

        if let Some(conn) = &self.connection {
            match conn.open_bi().await {
                Ok((mut send, mut recv)) => {
                    debug!("Control stream opened successfully to {}", self.publisher_id);

                    // Send CONTROL stream type
                    if let Err(e) = send.write_all(&[STREAM_TYPE_CONTROL]).await {
                        error!("Failed to send CONTROL stream type: {}", e);
                        self.handle_terminate(
                            anyhow!("Failed to send CONTROL stream type: {}", e)
                        ).await;
                        return;
                    }

                    debug!("Sent CONTROL stream type to {}", self.publisher_id);

                    // Store the send stream for later use
                    self.control_stream_send = Some(send);
                    self.state = SubscriberState::ControlStreamEstablished;

                    // Spawn task to handle control stream messages
                    let cmd_tx = self.cmd_tx.clone();
                    let stream_id = self.stream_id;
                    let publisher_id = self.publisher_id;

                    // Create a new task that processes the receive stream
                    tokio::spawn(async move {
                        let mut buffer = BytesMut::new();

                        loop {
                            match recv.read_chunk(65535, false).await {
                                Ok(Some(chunk)) => {
                                    buffer.extend(chunk.bytes);

                                    // Process any complete messages
                                    while buffer.len() >= 1 {
                                        // Check for SUBSCRIBE_OK message
                                        if buffer.len() >= 1 && buffer[0] == TYPE_SUBSCRIBE_OK {
                                            if buffer.len() >= 17 {
                                                debug!("Received SUBSCRIBE_OK for stream {}", stream_id);
                                                buffer.advance(17); // Consume the message

                                                // Signal successful subscription
                                                if
                                                    let Err(e) = cmd_tx.send(
                                                        SubscriberCommand::SubscribeOk
                                                    ).await
                                                {
                                                    error!("Failed to send SubscribeOk command: {}", e);
                                                }
                                            } else {
                                                // Wait for more data
                                                break;
                                            }
                                        } else {
                                            // Skip unknown message type
                                            debug!("Skipping unknown message type: {}", buffer[0]);
                                            buffer.advance(1);
                                        }
                                    }
                                }
                                Ok(None) => {
                                    debug!("Control stream from {} closed gracefully", publisher_id);
                                    return;
                                }
                                Err(e) => {
                                    error!("Error reading from control stream: {}", e);
                                    if
                                        let Err(e2) = cmd_tx.send(
                                            SubscriberCommand::Terminate(
                                                anyhow!("Control stream error: {}", e)
                                            )
                                        ).await
                                    {
                                        error!("Failed to send Terminate command: {}", e2);
                                    }
                                    return;
                                }
                            }
                        }
                    });

                    // Transition to next state
                    if let Err(e) = self.cmd_tx.send(SubscriberCommand::Subscribe).await {
                        error!("Failed to send Subscribe command: {}", e);
                        self.handle_terminate(anyhow!("Command channel error: {}", e)).await;
                    }
                }
                Err(e) => {
                    error!("Failed to open control stream: {}", e);
                    self.handle_terminate(anyhow!("Failed to open control stream: {}", e)).await;
                }
            }
        } else {
            error!("No connection available when trying to open control stream");
            self.handle_terminate(anyhow!("No connection available")).await;
        }
    }

    async fn handle_subscribe(&mut self) {
        info!(
            "Subscribing to stream {} in namespace {} from {}",
            self.stream_id,
            self.namespace,
            self.publisher_id
        );

        // First register with the engine
        match
            self.engine.subscribe_to_stream(
                self.publisher_id,
                self.stream_id,
                self.namespace.clone(),
                0, // Start sequence
                0, // Group ID - changed from 1 to 0 to receive all groups including init segments
                255 // Priority
            ).await
        {
            Ok(_) => {
                debug!("Successfully registered with engine for stream {}", self.stream_id);

                // Now send the subscribe message over the control stream
                if let Some(ref mut send) = self.control_stream_send {
                    let subscribe_msg = serialize_subscribe(
                        self.stream_id,
                        &self.namespace,
                        0, // Start sequence
                        0, // Group ID - changed from 1 to 0 to receive all groups including init segments
                        255 // Priority
                    );

                    match send.write_all(&subscribe_msg).await {
                        Ok(_) => {
                            debug!(
                                "Sent SUBSCRIBE message for stream {} in namespace {}",
                                self.stream_id,
                                self.namespace
                            );
                            self.state = SubscriberState::Subscribing;
                        }
                        Err(e) => {
                            error!("Failed to send SUBSCRIBE message: {}", e);
                            self.handle_terminate(
                                anyhow!("Failed to send SUBSCRIBE message: {}", e)
                            ).await;
                        }
                    }
                } else {
                    error!("No control stream available when trying to subscribe");
                    self.handle_terminate(anyhow!("No control stream available")).await;
                }
            }
            Err(e) => {
                error!("Failed to register with engine: {}", e);
                self.handle_terminate(anyhow!("Engine registration failed: {}", e)).await;
            }
        }
    }

    async fn handle_subscribe_ok(&mut self) {
        info!(
            "Received SUBSCRIBE_OK for stream {} in namespace {} from {}",
            self.stream_id,
            self.namespace,
            self.publisher_id
        );

        // Update state
        self.state = SubscriberState::Subscribed;

        // Proceed to open data stream
        if let Err(e) = self.cmd_tx.send(SubscriberCommand::OpenDataStream).await {
            error!("Failed to send OpenDataStream command: {}", e);
            self.handle_terminate(anyhow!("Command channel error: {}", e)).await;
        }
    }

    async fn handle_open_data_stream(&mut self) {
        info!(
            "Opening data stream to publisher {} for stream {}",
            self.publisher_id,
            self.stream_id
        );

        if let Some(conn) = &self.connection {
            match conn.open_bi().await {
                Ok((mut send, recv)) => {
                    debug!("Data stream opened successfully to {}", self.publisher_id);

                    // Send DATA stream type
                    if let Err(e) = send.write_all(&[STREAM_TYPE_DATA]).await {
                        error!("Failed to send DATA stream type: {}", e);
                        self.handle_terminate(
                            anyhow!("Failed to send DATA stream type: {}", e)
                        ).await;
                        return;
                    }

                    // Send stream ID as little-endian bytes
                    if let Err(e) = send.write_all(&self.stream_id.to_bytes_le()).await {
                        error!("Failed to send stream ID: {}", e);
                        self.handle_terminate(anyhow!("Failed to send stream ID: {}", e)).await;
                        return;
                    }

                    // Send namespace length as little-endian u16
                    let namespace_bytes = self.namespace.as_bytes();
                    let namespace_len = namespace_bytes.len() as u16;
                    if let Err(e) = send.write_all(&namespace_len.to_le_bytes()).await {
                        error!("Failed to send namespace length: {}", e);
                        self.handle_terminate(
                            anyhow!("Failed to send namespace length: {}", e)
                        ).await;
                        return;
                    }

                    // Send namespace
                    if let Err(e) = send.write_all(namespace_bytes).await {
                        error!("Failed to send namespace: {}", e);
                        self.handle_terminate(anyhow!("Failed to send namespace: {}", e)).await;
                        return;
                    }

                    debug!(
                        "Sent stream identification (id={}, namespace={}) to publisher",
                        self.stream_id,
                        self.namespace
                    );

                    // Use AsyncWriteExt::flush
                    if let Err(e) = send.flush().await {
                        error!("Failed to flush data stream: {}", e);
                        self.handle_terminate(anyhow!("Failed to flush data stream: {}", e)).await;
                        return;
                    }

                    // Spawn a task to read from the receive stream
                    let cmd_tx = self.cmd_tx.clone();
                    let stream_id = self.stream_id;

                    tokio::spawn(async move {
                        let mut recv = recv;
                        let mut buffer = BytesMut::new();

                        debug!("Started data stream receive task for {}", stream_id);

                        while let Ok(Some(chunk)) = recv.read_chunk(65535, false).await {
                            trace!("Received {} bytes on data stream", chunk.bytes.len());
                            buffer.extend_from_slice(&chunk.bytes);

                            // Try to deserialize objects while there's enough data
                            loop {
                                match deserialize_object(&mut buffer) {
                                    Ok(obj) => {
                                        // debug!(
                                        //     "Deserialized object: seq={}, group_id={}, size={} bytes",
                                        //     obj.sequence,
                                        //     obj.group_id,
                                        //     obj.data.len()
                                        // );

                                        if
                                            let Err(e) = cmd_tx.send(
                                                SubscriberCommand::ProcessObject(obj)
                                            ).await
                                        {
                                            error!("Failed to send object to actor: {}", e);
                                            return;
                                        }
                                    }
                                    Err(_) => {
                                        // Not enough data for a complete object, wait for more
                                        break;
                                    }
                                }
                            }
                        }

                        debug!("Data stream receive task completed for {}", stream_id);
                    });

                    // Store the send stream if we need it later (optional)
                    // self.data_stream = Some((send, recv));

                    info!("Successfully set up data stream for {}", self.stream_id);
                    self.state = SubscriberState::Subscribed;
                }
                Err(e) => {
                    error!("Failed to open data stream: {}", e);
                    self.handle_terminate(anyhow!("Failed to open data stream: {}", e)).await;
                }
            }
        } else {
            error!("No connection available when trying to open data stream");
            self.handle_terminate(anyhow!("No connection available")).await;
        }
    }

    async fn handle_process_object(&mut self, object: MoqObject) {
        trace!(
            "Processing received object: group={}, seq={}, size={} bytes",
            object.group_id,
            object.sequence,
            object.data.len()
        );

        match object.group_id {
            g if g == (MEDIA_TYPE_INIT as u32) => {
                info!("Processing init segment for stream {}", self.stream_id);

                // Create MediaInit struct with values from the stream announcement
                let init = MediaInit {
                    codec: self.announcement.codec.clone(),
                    mime_type: format!(
                        "video/{}",
                        self.announcement.codec.split('.').next().unwrap_or("mp4")
                    ),
                    width: self.announcement.resolution.0,
                    height: self.announcement.resolution.1,
                    frame_rate: self.announcement.framerate as f32,
                    bitrate: self.announcement.bitrate,
                    init_segment: object.data,
                };

                if let Err(e) = self.init_tx.send(init).await {
                    error!("Failed to send init segment to receiver: {}", e);
                    self.handle_terminate(anyhow!("Failed to send init segment: {}", e)).await;
                    return;
                }

                info!("Init segment received and forwarded for stream {}", self.stream_id);
            }
            g if g == (MEDIA_TYPE_VIDEO as u32) => {
                // info!(
                //     "Received video chunk: seq={}, size={} bytes for stream {}",
                //     object.sequence,
                //     object.data.len(),
                //     self.stream_id
                // );

                // Convert to VideoChunk
                match Self::object_to_video_chunk(&object) {
                    Ok(chunk) => {
                        if let Err(e) = self.chunk_tx.send(chunk).await {
                            debug!("Video chunk receiver closed: {}", e);
                            self.handle_terminate(
                                anyhow!("Video chunk receiver closed: {}", e)
                            ).await;
                            return;
                        }
                    }
                    Err(e) => {
                        error!("Failed to parse video chunk from object: {}", e);
                        // Don't terminate for a single failed chunk
                    }
                }
            }
            _ => {
                debug!("Received object with unknown group_id {}", object.group_id);
            }
        }
    }

    async fn handle_terminate(&mut self, error: anyhow::Error) {
        let error_msg = error.to_string();
        error!("Terminating subscriber actor for stream {}: {}", self.stream_id, error_msg);

        // Update state
        self.state = SubscriberState::Terminated(error_msg);

        // Close all streams and connections
        self.control_stream = None;
        self.control_stream_send = None;
        self.data_stream = None;
        self.connection = None;
    }

    /// Helper method to convert an object to a video chunk
    fn object_to_video_chunk(object: &MoqObject) -> Result<VideoChunk> {
        // Extract is_keyframe from priority (in a real implementation this would be more robust)
        let is_keyframe = object.priority >= 200;

        // Create the video chunk
        let chunk = VideoChunk {
            timestamp: object.timestamp,
            duration: 0, // This would be set based on actual duration
            is_keyframe,
            dependency_sequence: None, // In a real implementation, this would be set if needed
            data: object.data.clone(),
        };

        Ok(chunk)
    }
}

// Function to deserialize a MoqObject from a buffer
fn deserialize_object(buffer: &mut BytesMut) -> Result<MoqObject> {
    if buffer.len() < 4 {
        trace!("Buffer too small for length header: {} bytes", buffer.len());
        bail!("Buffer too small for length header");
    }

    // Read total length
    let mut length_bytes = [0u8; 4];
    length_bytes.copy_from_slice(&buffer[..4]);
    let total_length = u32::from_be_bytes(length_bytes) as usize;

    trace!(
        "Found object with length prefix of {} bytes, buffer size: {}",
        total_length,
        buffer.len()
    );

    // Check if we have the complete object
    if buffer.len() < 4 + total_length {
        trace!(
            "Incomplete object data: need {} bytes, have {} bytes",
            4 + total_length,
            buffer.len()
        );
        bail!("Incomplete object data");
    }

    // Skip total length prefix
    buffer.advance(4);

    // Take the object data
    let object_data = buffer.split_to(total_length);

    // Parse object fields using a cursor
    let mut cursor = Cursor::new(&object_data);

    // Read name length (u16)
    let name_len = u16::from_be_bytes([
        cursor.get_ref()[cursor.position() as usize],
        cursor.get_ref()[(cursor.position() as usize) + 1],
    ]);
    cursor.set_position(cursor.position() + 2);

    trace!("Name length: {} bytes", name_len);

    // Read name
    let name_end = (cursor.position() as usize) + (name_len as usize);
    if name_end > cursor.get_ref().len() {
        bail!("Name length exceeds buffer size");
    }

    let name = String::from_utf8_lossy(
        &cursor.get_ref()[cursor.position() as usize..name_end]
    ).to_string();
    cursor.set_position(name_end as u64);

    trace!("Read name: {}", name);

    // Read sequence (u64)
    let mut seq_bytes = [0u8; 8];
    cursor.read_exact(&mut seq_bytes)?;
    let sequence = u64::from_be_bytes(seq_bytes);

    // Read timestamp (u64)
    let mut ts_bytes = [0u8; 8];
    cursor.read_exact(&mut ts_bytes)?;
    let timestamp = u64::from_be_bytes(ts_bytes);

    // Read group_id (u32)
    let mut group_bytes = [0u8; 4];
    cursor.read_exact(&mut group_bytes)?;
    let group_id = u32::from_be_bytes(group_bytes);

    // Read priority (u8)
    let mut priority_byte = [0u8; 1];
    cursor.read_exact(&mut priority_byte)?;
    let priority = priority_byte[0];

    // Read data length (u32)
    let mut data_len_bytes = [0u8; 4];
    cursor.read_exact(&mut data_len_bytes)?;
    let data_len = u32::from_be_bytes(data_len_bytes) as usize;

    trace!("Data length: {} bytes", data_len);

    // Read data
    let pos = cursor.position() as usize;
    let data_end = pos + data_len;
    if data_end > cursor.get_ref().len() {
        bail!("Data length exceeds buffer size");
    }
    let data = cursor.get_ref()[pos..data_end].to_vec();

    Ok(MoqObject {
        name,
        sequence,
        timestamp,
        group_id,
        priority,
        data,
    })
}

// Create a helper function to subscribe to a video stream using the actor
pub async fn subscribe_to_video_stream(
    endpoint: Arc<Endpoint>,
    engine: Arc<MoqIrohEngine>,
    announcement: StreamAnnouncement
) -> Result<(mpsc::Receiver<MediaInit>, mpsc::Receiver<VideoChunk>)> {
    info!(
        "Creating subscriber actor for stream {} from {}",
        announcement.stream_id,
        announcement.sender_id
    );

    // Create the subscriber actor and await the result
    SubscriberActor::new(endpoint, engine, announcement).await
}
