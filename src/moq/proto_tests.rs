use super::*;
use bytes::{BytesMut, BufMut, Buf};
use uuid::Uuid;
use iroh::PublicKey;

#[test]
fn test_stream_announcement_serialization() {
    let announcement = StreamAnnouncement {
        stream_id: Uuid::new_v4(),
        sender_id: PublicKey::from_bytes(&[1u8; 32]).unwrap(),
        namespace: "/live/video".to_string(),
        codec: "h264".to_string(),
        resolution: (1920, 1080),
        framerate: 30,
        bitrate: 5000000,
        timestamp: 1234567890,
        relay_ids: vec!["relay1".to_string(), "relay2".to_string()],
    };

    // Serialize
    let serialized = serialize_announce(&announcement).unwrap();
    assert!(!serialized.is_empty());

    // Deserialize
    let deserialized = deserialize_announce(&serialized).unwrap();
    assert_eq!(announcement.stream_id, deserialized.stream_id);
    assert_eq!(announcement.sender_id, deserialized.sender_id);
    assert_eq!(announcement.namespace, deserialized.namespace);
    assert_eq!(announcement.codec, deserialized.codec);
    assert_eq!(announcement.resolution, deserialized.resolution);
    assert_eq!(announcement.framerate, deserialized.framerate);
    assert_eq!(announcement.bitrate, deserialized.bitrate);
    assert_eq!(announcement.timestamp, deserialized.timestamp);
    assert_eq!(announcement.relay_ids, deserialized.relay_ids);
}

#[test]
fn test_invalid_stream_announcement() {
    // Test with invalid JSON
    let invalid_data = b"not json";
    let result = deserialize_announce(invalid_data);
    assert!(result.is_err());

    // Test with empty data
    let empty_data = b"";
    let result = deserialize_announce(empty_data);
    assert!(result.is_err());
}

#[test]
fn test_subscribe_message_serialization() {
    let stream_id = Uuid::new_v4();
    let namespace = "/test/namespace";
    let start_sequence = 42;
    let group_id = MEDIA_TYPE_VIDEO as u32;
    let priority = 128;

    // Serialize
    let serialized = serialize_subscribe(stream_id, namespace, start_sequence, group_id, priority);
    assert_eq!(serialized[0], TYPE_SUBSCRIBE);

    // Deserialize
    let mut buffer = serialized;
    let (des_stream_id, des_namespace, des_start_seq, des_group_id, des_priority) = 
        deserialize_subscribe(&mut buffer).unwrap();

    assert_eq!(stream_id, des_stream_id);
    assert_eq!(namespace, des_namespace);
    assert_eq!(start_sequence, des_start_seq);
    assert_eq!(group_id, des_group_id);
    assert_eq!(priority, des_priority);
}

#[test]
fn test_subscribe_message_edge_cases() {
    // Test with empty namespace
    let stream_id = Uuid::new_v4();
    let serialized = serialize_subscribe(stream_id, "", 0, 0, 0);
    let mut buffer = serialized;
    let result = deserialize_subscribe(&mut buffer).unwrap();
    assert_eq!(result.1, "");

    // Test with max values
    let serialized = serialize_subscribe(stream_id, "test", u64::MAX, u32::MAX, u8::MAX);
    let mut buffer = serialized;
    let result = deserialize_subscribe(&mut buffer).unwrap();
    assert_eq!(result.2, u64::MAX);
    assert_eq!(result.3, u32::MAX);
    assert_eq!(result.4, u8::MAX);
}

#[test]
fn test_subscribe_message_incomplete() {
    // Test with incomplete header
    let mut buffer = BytesMut::new();
    buffer.put_u8(TYPE_SUBSCRIBE);
    buffer.put_u8(10); // namespace length but no more data
    let result = deserialize_subscribe(&mut buffer);
    assert!(result.is_err());

    // Test with incomplete body
    let mut buffer = BytesMut::new();
    buffer.put_u8(TYPE_SUBSCRIBE);
    buffer.put_u16(5);
    buffer.extend_from_slice(Uuid::new_v4().as_bytes());
    buffer.extend_from_slice(b"test"); // Missing one byte
    let result = deserialize_subscribe(&mut buffer);
    assert!(result.is_err());
}

#[test]
fn test_unsubscribe_message_serialization() {
    let stream_id = Uuid::new_v4();
    let namespace = "/test/stream";

    // Serialize
    let serialized = serialize_unsubscribe(stream_id, namespace);
    assert_eq!(serialized[0], TYPE_UNSUBSCRIBE);

    // Deserialize
    let mut buffer = serialized;
    let (des_stream_id, des_namespace) = deserialize_unsubscribe(&mut buffer).unwrap();
    assert_eq!(stream_id, des_stream_id);
    assert_eq!(namespace, des_namespace);
}

#[test]
fn test_cancel_message_serialization() {
    let sequence = 12345;

    // Serialize
    let serialized = serialize_cancel(sequence);
    assert_eq!(serialized[0], TYPE_CANCEL);

    // Deserialize
    let mut buffer = serialized;
    let des_sequence = deserialize_cancel(&mut buffer).unwrap();
    assert_eq!(sequence, des_sequence);
}

#[test]
fn test_request_message_serialization() {
    let stream_id = Uuid::new_v4();
    let namespace = "/test/request";
    let sequence = 99999;

    // Serialize
    let serialized = serialize_request(stream_id, namespace, sequence);
    assert_eq!(serialized[0], TYPE_REQUEST);

    // Deserialize
    let mut buffer = serialized;
    let (des_stream_id, des_namespace, des_sequence) = deserialize_request(&mut buffer).unwrap();
    assert_eq!(stream_id, des_stream_id);
    assert_eq!(namespace, des_namespace);
    assert_eq!(sequence, des_sequence);
}

#[test]
fn test_terminate_message_serialization() {
    // Test normal termination
    let serialized = serialize_terminate(REASON_NORMAL);
    assert_eq!(serialized[0], TYPE_TERMINATE);
    let mut buffer = serialized;
    let reason = deserialize_terminate(&mut buffer).unwrap();
    assert_eq!(reason, REASON_NORMAL);

    // Test error termination
    let serialized = serialize_terminate(REASON_ERROR);
    let mut buffer = serialized;
    let reason = deserialize_terminate(&mut buffer).unwrap();
    assert_eq!(reason, REASON_ERROR);
}

#[test]
fn test_heartbeat_message_serialization() {
    let timestamp = 1234567890123456;

    // Serialize
    let serialized = serialize_heartbeat(timestamp);
    assert_eq!(serialized[0], TYPE_HEARTBEAT);

    // Deserialize
    let mut buffer = serialized;
    let des_timestamp = deserialize_heartbeat(&mut buffer).unwrap();
    assert_eq!(timestamp, des_timestamp);
}

#[test]
fn test_subscribe_ok_message_serialization() {
    let stream_id = Uuid::new_v4();

    // Serialize
    let serialized = serialize_subscribe_ok(stream_id);
    assert_eq!(serialized[0], TYPE_SUBSCRIBE_OK);

    // Deserialize
    let mut buffer = serialized;
    let des_stream_id = deserialize_subscribe_ok(&mut buffer).unwrap();
    assert_eq!(stream_id, des_stream_id);
}

#[test]
fn test_moq_object_serialization() {
    let object = MoqObject {
        name: "/test/object".to_string(),
        sequence: 42,
        timestamp: 1234567890,
        group_id: MEDIA_TYPE_VIDEO as u32,
        priority: 255,
        data: vec![1, 2, 3, 4, 5],
    };

    // Serialize
    let serialized = serialize_object(&object);
    
    // Check the message has the length prefix
    let mut buffer = serialized;
    let total_length = (&buffer[..4]).get_u32() as usize;
    assert!(total_length > 0);
    buffer.advance(4);

    // Deserialize (create a new buffer with the message)
    let mut des_buffer = BytesMut::new();
    des_buffer.put_u32(total_length as u32);
    des_buffer.extend_from_slice(&buffer[..total_length]);
    
    let deserialized = deserialize_object(&mut des_buffer).unwrap();
    assert_eq!(object.name, deserialized.name);
    assert_eq!(object.sequence, deserialized.sequence);
    assert_eq!(object.timestamp, deserialized.timestamp);
    assert_eq!(object.group_id, deserialized.group_id);
    assert_eq!(object.priority, deserialized.priority);
    assert_eq!(object.data, deserialized.data);
}

#[test]
fn test_moq_object_type_flags() {
    // Test init segment
    let init_obj = MoqObject {
        name: "init".to_string(),
        sequence: 0,
        timestamp: 0,
        group_id: MEDIA_TYPE_INIT as u32,
        priority: 255,
        data: vec![1, 2, 3],
    };
    let serialized = serialize_object(&init_obj);
    assert_eq!(serialized[4], TYPE_OBJECT);
    assert_eq!(serialized[5], 0x01); // Init type flag

    // Test video keyframe
    let keyframe_obj = MoqObject {
        name: "keyframe".to_string(),
        sequence: 1,
        timestamp: 1000,
        group_id: MEDIA_TYPE_VIDEO as u32,
        priority: 255,
        data: vec![4, 5, 6],
    };
    let serialized = serialize_object(&keyframe_obj);
    assert_eq!(serialized[5], 0x02); // Keyframe type flag

    // Test video regular frame
    let regular_obj = MoqObject {
        name: "regular".to_string(),
        sequence: 2,
        timestamp: 2000,
        group_id: MEDIA_TYPE_VIDEO as u32,
        priority: 128,
        data: vec![7, 8, 9],
    };
    let serialized = serialize_object(&regular_obj);
    assert_eq!(serialized[5], 0x03); // Regular frame type flag

    // Test audio frame
    let audio_obj = MoqObject {
        name: "audio".to_string(),
        sequence: 3,
        timestamp: 3000,
        group_id: MEDIA_TYPE_AUDIO as u32,
        priority: 128,
        data: vec![10, 11, 12],
    };
    let serialized = serialize_object(&audio_obj);
    assert_eq!(serialized[5], 0x04); // Audio type flag
}

#[test]
fn test_video_chunk_conversion() {
    let chunk = VideoChunk {
        timestamp: 1000000,
        duration: 33333,
        is_keyframe: true,
        dependency_sequence: Some(10),
        data: vec![0xFF, 0xFE, 0xFD],
    };

    let object = MoqObject::from_video_chunk(
        "/video/chunk1".to_string(),
        42,
        chunk.clone(),
        MEDIA_TYPE_VIDEO as u32,
    );

    assert_eq!(object.name, "/video/chunk1");
    assert_eq!(object.sequence, 42);
    assert_eq!(object.timestamp, chunk.timestamp);
    assert_eq!(object.group_id, MEDIA_TYPE_VIDEO as u32);
    assert_eq!(object.priority, 255); // Keyframe priority

    // Convert back
    let converted = object.to_video_chunk().unwrap();
    assert_eq!(converted.timestamp, chunk.timestamp);
    assert_eq!(converted.duration, chunk.duration);
    assert_eq!(converted.is_keyframe, chunk.is_keyframe);
    assert_eq!(converted.dependency_sequence, chunk.dependency_sequence);
    assert_eq!(converted.data, chunk.data);
}

#[test]
fn test_video_chunk_no_dependency() {
    let chunk = VideoChunk {
        timestamp: 2000000,
        duration: 33333,
        is_keyframe: false,
        dependency_sequence: None,
        data: vec![0xAA, 0xBB, 0xCC],
    };

    let object = MoqObject::from_video_chunk(
        "/video/chunk2".to_string(),
        43,
        chunk.clone(),
        MEDIA_TYPE_VIDEO as u32,
    );

    assert_eq!(object.priority, 128); // Non-keyframe priority

    // Convert back
    let converted = object.to_video_chunk().unwrap();
    assert_eq!(converted.dependency_sequence, None);
}

#[test]
fn test_audio_chunk_conversion() {
    let chunk = AudioChunk {
        timestamp: 3000000,
        duration: 20000,
        is_key: true,
        data: vec![0x11, 0x22, 0x33, 0x44],
    };

    let object = MoqObject::from_audio_chunk(
        "/audio/chunk1".to_string(),
        44,
        chunk.clone(),
        MEDIA_TYPE_AUDIO as u32,
    );

    assert_eq!(object.name, "/audio/chunk1");
    assert_eq!(object.sequence, 44);
    assert_eq!(object.timestamp, chunk.timestamp);
    assert_eq!(object.group_id, MEDIA_TYPE_AUDIO as u32);
    assert_eq!(object.priority, 128);

    // Convert back
    let converted = object.to_audio_chunk().unwrap();
    assert_eq!(converted.timestamp, chunk.timestamp);
    assert_eq!(converted.duration, chunk.duration);
    assert_eq!(converted.is_key, chunk.is_key);
    assert_eq!(converted.data, chunk.data);
}

#[test]
fn test_audio_chunk_wrong_group_id() {
    let object = MoqObject {
        name: "test".to_string(),
        sequence: 1,
        timestamp: 0,
        group_id: MEDIA_TYPE_VIDEO as u32, // Wrong group ID
        priority: 128,
        data: vec![1, 2, 3],
    };

    let result = object.to_audio_chunk();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not audio"));
}

#[test]
fn test_media_init_serialization() {
    let init = MediaInit {
        codec: "avc1.64001f".to_string(),
        mime_type: "video/mp4".to_string(),
        width: 1920,
        height: 1080,
        frame_rate: 29.97,
        bitrate: 5000000,
        init_segment: vec![0x00, 0x00, 0x00, 0x20],
    };

    let serialized = serde_json::to_vec(&init).unwrap();
    let deserialized: MediaInit = serde_json::from_slice(&serialized).unwrap();

    assert_eq!(init.codec, deserialized.codec);
    assert_eq!(init.mime_type, deserialized.mime_type);
    assert_eq!(init.width, deserialized.width);
    assert_eq!(init.height, deserialized.height);
    assert_eq!(init.frame_rate, deserialized.frame_rate);
    assert_eq!(init.bitrate, deserialized.bitrate);
    assert_eq!(init.init_segment, deserialized.init_segment);
}

#[test]
fn test_audio_init_serialization() {
    let init = AudioInit {
        codec: "opus".to_string(),
        mime_type: "audio/opus".to_string(),
        sample_rate: 48000,
        channels: 2,
        bitrate: 128000,
        init_segment: vec![0x01, 0x02, 0x03],
    };

    let serialized = serde_json::to_vec(&init).unwrap();
    let deserialized: AudioInit = serde_json::from_slice(&serialized).unwrap();

    assert_eq!(init.codec, deserialized.codec);
    assert_eq!(init.mime_type, deserialized.mime_type);
    assert_eq!(init.sample_rate, deserialized.sample_rate);
    assert_eq!(init.channels, deserialized.channels);
    assert_eq!(init.bitrate, deserialized.bitrate);
    assert_eq!(init.init_segment, deserialized.init_segment);
}

#[test]
fn test_topic_hash() {
    let topic = b"test-topic";
    let hash1 = topic_hash(topic);
    let hash2 = topic_hash(topic);
    assert_eq!(hash1, hash2); // Should be deterministic

    let different_topic = b"different-topic";
    let hash3 = topic_hash(different_topic);
    assert_ne!(hash1, hash3); // Different topics should have different hashes
}

#[test]
fn test_buffer_boundary_conditions() {
    // Test deserializing with exact buffer size
    let mut buffer = BytesMut::new();
    buffer.put_u8(TYPE_CANCEL);
    buffer.put_u64(12345);
    
    let result = deserialize_cancel(&mut buffer);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 12345);
    assert_eq!(buffer.len(), 0); // Buffer should be consumed

    // Test deserializing with extra data
    let mut buffer = BytesMut::new();
    buffer.put_u8(TYPE_TERMINATE);
    buffer.put_u8(REASON_ERROR);
    buffer.put_u8(0xFF); // Extra byte
    
    let result = deserialize_terminate(&mut buffer);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), REASON_ERROR);
    assert_eq!(buffer.len(), 1); // Extra byte should remain
}

#[test] 
fn test_invalid_utf8_handling() {
    // Test invalid UTF-8 in namespace
    let mut buffer = BytesMut::new();
    buffer.put_u8(TYPE_UNSUBSCRIBE);
    buffer.extend_from_slice(Uuid::new_v4().as_bytes());
    buffer.put_u16(4);
    buffer.extend_from_slice(&[0xFF, 0xFE, 0xFD, 0xFC]); // Invalid UTF-8
    
    let result = deserialize_unsubscribe(&mut buffer);
    assert!(result.is_err());
}

#[test]
fn test_large_object_serialization() {
    // Test with large data payload
    let large_data = vec![0xAB; 1_000_000]; // 1MB of data
    let object = MoqObject {
        name: "/large/object".to_string(),
        sequence: 999999,
        timestamp: u64::MAX,
        group_id: MEDIA_TYPE_VIDEO as u32,
        priority: 200,
        data: large_data.clone(),
    };

    let serialized = serialize_object(&object);
    let mut buffer = serialized;
    let deserialized = deserialize_object(&mut buffer).unwrap();
    
    assert_eq!(deserialized.data.len(), large_data.len());
    assert_eq!(deserialized.data, large_data);
}