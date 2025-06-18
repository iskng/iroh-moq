use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId, Throughput};
use bytes::{BytesMut, BufMut, Buf};
use iroh_moq::moq::proto::{
    MoqObject, VideoChunk, AudioChunk, serialize_object, deserialize_object,
    serialize_subscribe, deserialize_subscribe, MEDIA_TYPE_VIDEO, MEDIA_TYPE_AUDIO,
};
use std::time::Duration;
use uuid::Uuid;

fn bench_object_serialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("object_serialization");
    
    for size in [100, 1000, 10000, 100000, 1000000].iter() {
        let object = MoqObject {
            name: "/test/object".to_string(),
            sequence: 42,
            timestamp: 1234567890,
            group_id: MEDIA_TYPE_VIDEO as u32,
            priority: 255,
            data: vec![0xFF; *size],
        };
        
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, _| {
            b.iter(|| {
                let serialized = serialize_object(black_box(&object));
                black_box(serialized);
            });
        });
    }
    
    group.finish();
}

fn bench_object_deserialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("object_deserialization");
    
    for size in [100, 1000, 10000, 100000, 1000000].iter() {
        let object = MoqObject {
            name: "/test/object".to_string(),
            sequence: 42,
            timestamp: 1234567890,
            group_id: MEDIA_TYPE_VIDEO as u32,
            priority: 255,
            data: vec![0xFF; *size],
        };
        
        let serialized = serialize_object(&object);
        
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &serialized, |b, serialized| {
            b.iter(|| {
                let mut buffer = serialized.clone();
                let deserialized = deserialize_object(black_box(&mut buffer)).unwrap();
                black_box(deserialized);
            });
        });
    }
    
    group.finish();
}

fn bench_video_chunk_conversion(c: &mut Criterion) {
    let mut group = c.benchmark_group("video_chunk_conversion");
    
    for size in [1920*1080, 1280*720, 640*480].iter() {
        let chunk = VideoChunk {
            timestamp: 1000000,
            duration: 33333,
            is_keyframe: true,
            dependency_sequence: Some(10),
            data: vec![0xFF; *size],
        };
        
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, _| {
            b.iter(|| {
                let object = MoqObject::from_video_chunk(
                    "/video/chunk".to_string(),
                    42,
                    black_box(chunk.clone()),
                    MEDIA_TYPE_VIDEO as u32,
                );
                let _converted = object.to_video_chunk().unwrap();
            });
        });
    }
    
    group.finish();
}

fn bench_control_message_serialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("control_messages");
    
    group.bench_function("serialize_subscribe", |b| {
        let stream_id = Uuid::new_v4();
        let namespace = "/test/namespace/with/longer/path";
        b.iter(|| {
            let serialized = serialize_subscribe(
                black_box(stream_id),
                black_box(namespace),
                black_box(12345),
                black_box(1),
                black_box(128),
            );
            black_box(serialized);
        });
    });
    
    group.bench_function("deserialize_subscribe", |b| {
        let stream_id = Uuid::new_v4();
        let namespace = "/test/namespace/with/longer/path";
        let serialized = serialize_subscribe(stream_id, namespace, 12345, 1, 128);
        
        b.iter(|| {
            let mut buffer = serialized.clone();
            let result = deserialize_subscribe(black_box(&mut buffer)).unwrap();
            black_box(result);
        });
    });
    
    group.finish();
}

fn bench_buffer_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("buffer_operations");
    
    // Benchmark BytesMut operations
    group.bench_function("bytesmut_write_1mb", |b| {
        b.iter(|| {
            let mut buffer = BytesMut::with_capacity(1_000_000);
            for _ in 0..1000 {
                buffer.put_slice(&[0xFF; 1000]);
            }
            black_box(buffer);
        });
    });
    
    group.bench_function("bytesmut_read_1mb", |b| {
        let mut buffer = BytesMut::with_capacity(1_000_000);
        for _ in 0..1000 {
            buffer.put_slice(&[0xFF; 1000]);
        }
        
        b.iter(|| {
            let mut buf = buffer.clone();
            while buf.remaining() > 0 {
                let byte = buf.get_u8();
                black_box(byte);
            }
        });
    });
    
    group.finish();
}

fn bench_concurrent_object_processing(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_processing");
    group.measurement_time(Duration::from_secs(10));
    
    group.bench_function("parallel_serialization", |b| {
        use rayon::prelude::*;
        
        let objects: Vec<MoqObject> = (0..100)
            .map(|i| MoqObject {
                name: format!("/test/object/{}", i),
                sequence: i as u64,
                timestamp: 1234567890 + i as u64,
                group_id: MEDIA_TYPE_VIDEO as u32,
                priority: if i % 10 == 0 { 255 } else { 128 },
                data: vec![0xFF; 10000],
            })
            .collect();
        
        b.iter(|| {
            let results: Vec<_> = objects
                .par_iter()
                .map(|obj| serialize_object(black_box(obj)))
                .collect();
            black_box(results);
        });
    });
    
    group.finish();
}

fn bench_retransmission_buffer_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("retransmission_buffer");
    
    for buffer_size in [10, 50, 100, 500].iter() {
        // Simulate a retransmission buffer with objects
        let objects: Vec<MoqObject> = (0..*buffer_size)
            .map(|i| MoqObject {
                name: format!("/test/object/{}", i),
                sequence: i as u64,
                timestamp: 1234567890 + i as u64,
                group_id: MEDIA_TYPE_VIDEO as u32,
                priority: 128,
                data: vec![0xFF; 1000],
            })
            .collect();
        
        group.bench_with_input(BenchmarkId::from_parameter(buffer_size), buffer_size, |b, _| {
            b.iter(|| {
                // Linear search simulation (current implementation)
                let target_seq = (*buffer_size / 2) as u64;
                let found = objects.iter().find(|obj| obj.sequence == target_seq);
                black_box(found);
            });
        });
    }
    
    group.finish();
}

fn bench_audio_chunk_processing(c: &mut Criterion) {
    let mut group = c.benchmark_group("audio_chunk_processing");
    
    let chunk = AudioChunk {
        timestamp: 1000000,
        duration: 20000,
        is_key: true,
        data: vec![0x11; 960], // Typical Opus frame size
    };
    
    group.bench_function("audio_to_moq_object", |b| {
        b.iter(|| {
            let object = MoqObject::from_audio_chunk(
                "/audio/chunk".to_string(),
                42,
                black_box(chunk.clone()),
                MEDIA_TYPE_AUDIO as u32,
            );
            black_box(object);
        });
    });
    
    group.bench_function("moq_object_to_audio", |b| {
        let object = MoqObject::from_audio_chunk(
            "/audio/chunk".to_string(),
            42,
            chunk.clone(),
            MEDIA_TYPE_AUDIO as u32,
        );
        
        b.iter(|| {
            let converted = object.to_audio_chunk().unwrap();
            black_box(converted);
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    bench_object_serialization,
    bench_object_deserialization,
    bench_video_chunk_conversion,
    bench_control_message_serialization,
    bench_buffer_operations,
    bench_concurrent_object_processing,
    bench_retransmission_buffer_lookup,
    bench_audio_chunk_processing
);

criterion_main!(benches);