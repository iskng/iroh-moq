use anyhow::{anyhow, bail, Result};
use blake3;
use bytes::{Buf, BufMut, BytesMut};
use iroh::PublicKey;
use serde::{Deserialize, Serialize};
use tracing::info;
use uuid::Uuid;

// Protocol Constants
pub const ALPN: &[u8] = b"iroh-moq/v1";
pub const ANNOUNCE_TOPIC: &[u8] = b"moq-iroh-announce";

// Control Message Types
pub const TYPE_SUBSCRIBE: u8 = 0x01;
pub const TYPE_UNSUBSCRIBE: u8 = 0x02;
pub const TYPE_CANCEL: u8 = 0x03;
pub const TYPE_REQUEST: u8 = 0x04;
pub const TYPE_TERMINATE: u8 = 0x05;
pub const TYPE_HEARTBEAT: u8 = 0x06;
pub const TYPE_ANNOUNCE: u8 = 0x07;
pub const TYPE_SUBSCRIBE_OK: u8 = 0x08;

// Data Message Types
pub const TYPE_OBJECT: u8 = 0x10;

// Termination Reason Codes
pub const REASON_NORMAL: u8 = 0;
pub const REASON_ERROR: u8 = 1;

// Media-specific constants - Assign distinct values
pub const MEDIA_TYPE_INIT: u8 = 0x00;
pub const MEDIA_TYPE_VIDEO: u8 = 0x01;
pub const MEDIA_TYPE_AUDIO: u8 = 0x02;

// Special Group ID to signal End-of-Segment (EOS) within a stream
pub const MEDIA_TYPE_EOF: u32 = u32::MAX;

// Gossip Announcement
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StreamAnnouncement {
    pub stream_id: Uuid,        // Unique identifier for the stream
    pub sender_id: PublicKey,   // NodeId of the sender (serialized)
    pub namespace: String,      // e.g., "/live/video"
    pub codec: String,          // e.g., "h264"
    pub resolution: (u32, u32), // Width, height
    pub framerate: u32,         // Frames per second
    pub bitrate: u32,           // Bits per second
    pub timestamp: u64,         // Start time (microseconds since epoch)
    pub relay_ids: Vec<String>, // Optional relay NodeIds
}

// Data Object
#[derive(Debug, Clone)]
pub struct MoqObject {
    pub name: String,   // e.g., "/live/video/chunk1"
    pub sequence: u64,  // Sequence number for ordering
    pub timestamp: u64, // Timestamp (microseconds)
    pub group_id: u32,  // Track identifier (e.g., 1=video, 2=audio)
    pub priority: u8,   // Priority (0-255, higher is more urgent)
    pub data: Vec<u8>,  // Compressed chunk data
}

// Relay Information
#[derive(Debug, Clone)]
pub struct MoqRelay {
    pub relay_id: String,  // NodeId of the relay
    pub stream_id: Uuid,   // Stream being relayed
    pub namespace: String, // Namespace being relayed
}

// Media-specific structures
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaInit {
    pub codec: String,     // e.g. "avc1.64001f" for H.264
    pub mime_type: String, // e.g. "video/mp4"
    pub width: u32,
    pub height: u32,
    pub frame_rate: f32,
    pub bitrate: u32,
    pub init_segment: Vec<u8>, // MP4/CMAF initialization segment
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoChunk {
    pub timestamp: u64,                   // Presentation timestamp
    pub duration: u32,                    // Duration in timescale units
    pub is_keyframe: bool,                // Whether this is a keyframe/IDR
    pub dependency_sequence: Option<u64>, // Sequence number this chunk depends on
    pub data: Vec<u8>,                    // Actual video data (e.g. MP4 fragment)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioInit {
    pub codec: String,         // e.g. "opus" or "aac"
    pub mime_type: String,     // e.g. "audio/opus" or "audio/mp4"
    pub sample_rate: u32,      // Samples per second (Hz)
    pub channels: u8,          // Number of audio channels (1 = mono, 2 = stereo, etc.)
    pub bitrate: u32,          // Target bitrate in bits per second
    pub init_segment: Vec<u8>, // Codec specific config (ASC, Opus header, etc.)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioChunk {
    pub timestamp: u64, // Presentation timestamp in microseconds
    pub duration: u32,  // Duration in microseconds
    pub is_key: bool,   // For codecs where some frames are key (e.g. AAC with ADTS)
    pub data: Vec<u8>,  // Encoded audio frame payload
}

impl MoqObject {
    /// Create a new video object from a video chunk
    pub fn from_video_chunk(
        name: String,
        sequence: u64,
        chunk: VideoChunk,
        _group_id: u32,
    ) -> Self {
        // Create a buffer to serialize the video chunk
        let mut buffer = BytesMut::new();

        // Serialize the metadata
        buffer.put_u64(chunk.timestamp);
        buffer.put_u32(chunk.duration);
        buffer.put_u8(if chunk.is_keyframe { 1 } else { 0 });
        if let Some(dep_seq) = chunk.dependency_sequence {
            buffer.put_u8(1);
            buffer.put_u64(dep_seq);
        } else {
            buffer.put_u8(0);
            // Don't write 8 bytes when there's no dependency sequence
        }

        // Add the actual video data
        buffer.extend_from_slice(&chunk.data);

        Self {
            name,
            sequence,
            timestamp: chunk.timestamp,
            group_id: MEDIA_TYPE_VIDEO as u32, // Always use the constant
            priority: if chunk.is_keyframe { 255 } else { 128 },
            data: buffer.to_vec(),
        }
    }

    /// Try to convert this object into a video chunk
    pub fn to_video_chunk(&self) -> Result<VideoChunk> {
        let mut buffer = BytesMut::from(&self.data[..]);

        if buffer.len() < 13 {
            // Minimum size for metadata
            bail!("Video chunk data too small");
        }

        let timestamp = buffer.get_u64();
        let duration = buffer.get_u32();
        let is_keyframe = buffer.get_u8() == 1;

        let dependency_sequence = if buffer.get_u8() == 1 {
            if buffer.len() < 8 {
                bail!("Invalid dependency sequence data");
            }
            Some(buffer.get_u64())
        } else {
            None
        };

        // The remaining buffer is the video data
        let data = buffer.to_vec();

        Ok(VideoChunk {
            timestamp,
            duration,
            is_keyframe,
            dependency_sequence,
            data,
        })
    }

    /// Create a new audio object from an `AudioChunk`.
    pub fn from_audio_chunk(
        name: String,
        sequence: u64,
        chunk: AudioChunk,
        _group_id: u32,
    ) -> Self {
        let mut buffer = BytesMut::new();

        // Serialize minimal metadata (timestamp, duration, key flag)
        buffer.put_u64(chunk.timestamp);
        buffer.put_u32(chunk.duration);
        buffer.put_u8(if chunk.is_key { 1 } else { 0 });
        // Payload
        buffer.extend_from_slice(&chunk.data);

        Self {
            name,
            sequence,
            timestamp: chunk.timestamp,
            group_id: MEDIA_TYPE_AUDIO as u32,
            priority: 128,
            data: buffer.to_vec(),
        }
    }

    /// Try to convert this object into an `AudioChunk` if it is audio.
    pub fn to_audio_chunk(&self) -> Result<AudioChunk> {
        if self.group_id != (MEDIA_TYPE_AUDIO as u32) {
            bail!("MoqObject is not audio");
        }
        let mut buf = BytesMut::from(&self.data[..]);
        if buf.len() < 13 {
            bail!("Audio chunk data too small");
        }
        let timestamp = buf.get_u64();
        let duration = buf.get_u32();
        let is_key = buf.get_u8() == 1;
        let data = buf.to_vec();
        Ok(AudioChunk {
            timestamp,
            duration,
            is_key,
            data,
        })
    }
}

// Serialization Functions
pub fn serialize_announce(announcement: &StreamAnnouncement) -> Result<Vec<u8>> {
    serde_json::to_vec(announcement).map_err(|e| anyhow!("Serialization failed: {}", e))
}

pub fn deserialize_announce(bytes: &[u8]) -> Result<StreamAnnouncement> {
    serde_json::from_slice(bytes).map_err(|e| anyhow!("Deserialization failed: {}", e))
}

pub fn serialize_subscribe(
    stream_id: Uuid,
    namespace: &str,
    start_sequence: u64,
    group_id: u32,
    priority: u8,
) -> BytesMut {
    let mut buffer = BytesMut::new();
    buffer.put_u8(TYPE_SUBSCRIBE);
    let ns_bytes = namespace.as_bytes();
    buffer.put_u16(ns_bytes.len() as u16);
    buffer.extend_from_slice(stream_id.as_bytes());
    buffer.extend_from_slice(ns_bytes);
    buffer.put_u64(start_sequence);
    buffer.put_u32(group_id);
    buffer.put_u8(priority);
    buffer
}

pub fn deserialize_subscribe(buffer: &mut BytesMut) -> Result<(Uuid, String, u64, u32, u8)> {
    if buffer.len() < 3 {
        // type + namespace length (2 bytes)
        bail!("Incomplete SUBSCRIBE message header");
    }
    buffer.advance(1); // Skip type
    let ns_len = buffer.get_u16() as usize;

    if buffer.len() < 16 + ns_len + 13 {
        // uuid + namespace + params
        bail!("Incomplete SUBSCRIBE message body");
    }

    let stream_id = Uuid::from_slice(&buffer[..16])?;
    buffer.advance(16);

    let namespace = String::from_utf8(buffer[..ns_len].to_vec())?;
    buffer.advance(ns_len);

    let start_sequence = buffer.get_u64();
    let group_id = buffer.get_u32();
    let priority = buffer.get_u8();

    Ok((stream_id, namespace, start_sequence, group_id, priority))
}

pub fn serialize_unsubscribe(stream_id: Uuid, namespace: &str) -> BytesMut {
    let mut buffer = BytesMut::new();
    buffer.put_u8(TYPE_UNSUBSCRIBE);
    buffer.extend_from_slice(stream_id.as_bytes());
    let ns_bytes = namespace.as_bytes();
    buffer.put_u16(ns_bytes.len() as u16);
    buffer.extend_from_slice(ns_bytes);
    buffer
}

pub fn deserialize_unsubscribe(buffer: &mut BytesMut) -> Result<(Uuid, String)> {
    if buffer.len() < 19 {
        bail!("Incomplete UNSUBSCRIBE message");
    }
    buffer.advance(1);
    let stream_id = Uuid::from_slice(&buffer[..16])?;
    buffer.advance(16);
    let ns_len = buffer.get_u16() as usize;
    if buffer.len() < ns_len {
        bail!("Incomplete namespace");
    }
    let namespace = String::from_utf8(buffer[..ns_len].to_vec())?;
    buffer.advance(ns_len);
    Ok((stream_id, namespace))
}

pub fn serialize_cancel(sequence: u64) -> BytesMut {
    let mut buffer = BytesMut::new();
    buffer.put_u8(TYPE_CANCEL);
    buffer.put_u64(sequence);
    buffer
}

pub fn deserialize_cancel(buffer: &mut BytesMut) -> Result<u64> {
    if buffer.len() < 9 {
        bail!("Incomplete CANCEL message");
    }
    buffer.advance(1);
    Ok(buffer.get_u64())
}

pub fn serialize_request(stream_id: Uuid, namespace: &str, sequence: u64) -> BytesMut {
    let mut buffer = BytesMut::new();
    buffer.put_u8(TYPE_REQUEST);
    buffer.extend_from_slice(stream_id.as_bytes());
    let ns_bytes = namespace.as_bytes();
    buffer.put_u16(ns_bytes.len() as u16);
    buffer.extend_from_slice(ns_bytes);
    buffer.put_u64(sequence);
    buffer
}

pub fn deserialize_request(buffer: &mut BytesMut) -> Result<(Uuid, String, u64)> {
    if buffer.len() < 27 {
        // type(1) + uuid(16) + ns_len(2) + min_ns(0) + seq(8)
        bail!("Incomplete REQUEST message");
    }
    buffer.advance(1);
    let stream_id = Uuid::from_slice(&buffer[..16])?;
    buffer.advance(16);
    let ns_len = buffer.get_u16() as usize;
    if buffer.len() < ns_len + 8 {
        bail!("Incomplete namespace or sequence");
    }
    let namespace = String::from_utf8(buffer[..ns_len].to_vec())?;
    buffer.advance(ns_len);
    let sequence = buffer.get_u64();
    Ok((stream_id, namespace, sequence))
}

pub fn serialize_terminate(reason: u8) -> BytesMut {
    let mut buffer = BytesMut::new();
    buffer.put_u8(TYPE_TERMINATE);
    buffer.put_u8(reason);
    buffer
}

pub fn deserialize_terminate(buffer: &mut BytesMut) -> Result<u8> {
    if buffer.len() < 2 {
        bail!("Incomplete TERMINATE message");
    }
    buffer.advance(1);
    Ok(buffer.get_u8())
}

pub fn serialize_heartbeat(timestamp: u64) -> BytesMut {
    let mut buffer = BytesMut::new();
    buffer.put_u8(TYPE_HEARTBEAT);
    buffer.put_u64(timestamp);
    buffer
}

pub fn deserialize_heartbeat(buffer: &mut BytesMut) -> Result<u64> {
    if buffer.len() < 9 {
        bail!("Incomplete HEARTBEAT message");
    }
    buffer.advance(1);
    Ok(buffer.get_u64())
}

pub fn serialize_object(object: &MoqObject) -> BytesMut {
    let mut inner_buffer = BytesMut::new();

    // Add TYPE_OBJECT marker first
    inner_buffer.put_u8(TYPE_OBJECT);

    // Fixed Header Fields
    // Object Type/Flags (1 byte)
    let type_flags = match object.group_id {
        g if g == (MEDIA_TYPE_INIT as u32) => 0x01, // Init segment
        g if g == (MEDIA_TYPE_VIDEO as u32) => {
            if object.priority == 255 {
                0x02
            } else {
                0x03
            } // Key frame vs Regular frame
        }
        g if g == (MEDIA_TYPE_AUDIO as u32) => 0x04, // Audio frame
        _ => 0x00,                                   // Unknown type
    };
    inner_buffer.put_u8(type_flags);

    // Timestamp (8 bytes, network order)
    inner_buffer.put_u64(object.timestamp);

    // Extended Metadata Fields (TLV format)
    // Sequence number (Type=1)
    inner_buffer.put_u8(0x01); // Type
    inner_buffer.put_u8(8); // Length
    inner_buffer.put_u64(object.sequence);

    // Group ID (Type=2)
    inner_buffer.put_u8(0x02); // Type
    inner_buffer.put_u8(4); // Length
    inner_buffer.put_u32(object.group_id);

    // Priority (Type=3)
    inner_buffer.put_u8(0x03); // Type
    inner_buffer.put_u8(1); // Length
    inner_buffer.put_u8(object.priority);

    // Name (Type=4)
    let name_bytes = object.name.as_bytes();
    inner_buffer.put_u8(0x04); // Type
    inner_buffer.put_u8(name_bytes.len() as u8);
    inner_buffer.extend_from_slice(name_bytes);

    // End of metadata marker (Type=0)
    inner_buffer.put_u8(0x00);

    // Payload Length (4 bytes, network order)
    inner_buffer.put_u32(object.data.len() as u32);

    // Payload
    inner_buffer.extend_from_slice(&object.data);

    // Create final buffer with total message length prefix
    let mut final_buffer = BytesMut::new();
    final_buffer.put_u32(inner_buffer.len() as u32);
    final_buffer.extend_from_slice(&inner_buffer);

    // info!("Serialized object in {:?} us", start_time.elapsed().unwrap().as_micros());
    final_buffer
}

pub fn deserialize_object(buf: &mut BytesMut) -> Result<MoqObject> {
    // First read the total message length
    if buf.len() < 4 {
        bail!("Buffer too short for message length");
    }
    let total_length = buf.get_u32() as usize;

    // Check if we have enough data for the complete message
    if buf.len() < total_length {
        bail!("Buffer too short for complete message");
    }

    // Verify message type
    let msg_type = buf.get_u8();
    if msg_type != TYPE_OBJECT {
        bail!(
            "Invalid message type: {:#x}, expected {:#x}",
            msg_type,
            TYPE_OBJECT
        );
    }

    // Need at least type and timestamp (9 bytes)
    if buf.len() < 9 {
        bail!("Buffer too short for basic header");
    }

    // Read fixed header fields
    let type_flags = buf.get_u8();
    let timestamp = buf.get_u64();

    // Initialize object fields with defaults
    let mut sequence = 0;
    let mut group_id = match type_flags {
        0x01 => MEDIA_TYPE_INIT as u32,
        0x02 | 0x03 => MEDIA_TYPE_VIDEO as u32,
        0x04 => MEDIA_TYPE_AUDIO as u32,
        _ => 0,
    };
    let mut priority = if type_flags == 0x02 { 255 } else { 128 };
    let mut name = String::new();

    // Process TLV metadata fields
    loop {
        if buf.is_empty() {
            bail!("Unexpected end of buffer in metadata");
        }

        let tlv_type = buf.get_u8();
        if tlv_type == 0 {
            break; // End of metadata
        }

        if buf.is_empty() {
            bail!("Missing length in TLV");
        }
        let length = buf.get_u8() as usize;

        if buf.len() < length {
            bail!("Buffer too short for TLV value");
        }

        match tlv_type {
            1 => {
                sequence = buf.get_u64();
            }
            2 => {
                group_id = buf.get_u32();
            }
            3 => {
                priority = buf.get_u8();
            }
            4 => {
                name = String::from_utf8(buf[..length].to_vec())?;
                buf.advance(length);
            }
            _ => buf.advance(length), // Skip unknown TLV
        }
    }

    // Read payload length
    if buf.len() < 4 {
        bail!("Buffer too short for payload length");
    }
    let payload_len = buf.get_u32() as usize;

    // Read payload
    if buf.len() < payload_len {
        bail!("Buffer too short for payload");
    }
    let data = buf[..payload_len].to_vec();
    buf.advance(payload_len);

    info!(
        "Deserialized object with type_flags={:#x}, group_id={}, sequence={}, total_length={}",
        type_flags, group_id, sequence, total_length
    );

    Ok(MoqObject {
        name,
        sequence,
        timestamp,
        group_id,
        priority,
        data,
    })
}

pub fn serialize_subscribe_ok(stream_id: Uuid) -> BytesMut {
    let mut buffer = BytesMut::new();
    buffer.put_u8(TYPE_SUBSCRIBE_OK);
    buffer.extend_from_slice(stream_id.as_bytes());
    buffer
}

pub fn deserialize_subscribe_ok(buffer: &mut BytesMut) -> Result<Uuid> {
    if buffer.len() < 17 {
        bail!("Incomplete SUBSCRIBE_OK message");
    }
    buffer.advance(1); // Skip type
    let stream_id = Uuid::from_slice(&buffer[..16])?;
    buffer.advance(16);
    Ok(stream_id)
}

// Utility Functions
pub fn topic_hash(topic: &[u8]) -> blake3::Hash {
    blake3::hash(topic)
}
