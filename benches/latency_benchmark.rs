use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use iroh_moq::moq::proto::{
    MoqObject, serialize_object, deserialize_object,
    serialize_subscribe, deserialize_subscribe,
    serialize_request, deserialize_request,
    serialize_heartbeat, deserialize_heartbeat,
    MEDIA_TYPE_VIDEO,
};
use std::time::Instant;
use tokio::runtime::Runtime;
use uuid::Uuid;

/// Measure the round-trip time for various protocol operations
fn bench_protocol_round_trip(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol_round_trip");
    
    // Subscribe message round trip
    group.bench_function("subscribe_round_trip", |b| {
        let stream_id = Uuid::new_v4();
        let namespace = "/test/namespace";
        
        b.iter(|| {
            let start = Instant::now();
            
            // Serialize
            let serialized = serialize_subscribe(
                stream_id,
                namespace,
                12345,
                MEDIA_TYPE_VIDEO as u32,
                128,
            );
            
            // Simulate network transfer (in real scenario this would be QUIC send/receive)
            black_box(&serialized);
            
            // Deserialize
            let mut buffer = serialized;
            let _result = deserialize_subscribe(&mut buffer).unwrap();
            
            start.elapsed()
        });
    });
    
    // Request message round trip
    group.bench_function("request_round_trip", |b| {
        let stream_id = Uuid::new_v4();
        let namespace = "/test/namespace";
        
        b.iter(|| {
            let start = Instant::now();
            
            let serialized = serialize_request(stream_id, namespace, 42);
            black_box(&serialized);
            
            let mut buffer = serialized;
            let _result = deserialize_request(&mut buffer).unwrap();
            
            start.elapsed()
        });
    });
    
    // Heartbeat round trip
    group.bench_function("heartbeat_round_trip", |b| {
        b.iter(|| {
            let start = Instant::now();
            
            let timestamp = 1234567890;
            let serialized = serialize_heartbeat(timestamp);
            black_box(&serialized);
            
            let mut buffer = serialized;
            let _result = deserialize_heartbeat(&mut buffer).unwrap();
            
            start.elapsed()
        });
    });
    
    group.finish();
}

/// Measure latency for different sizes of video chunks
fn bench_video_chunk_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("video_chunk_latency");
    
    // Different video resolutions
    let resolutions = [
        ("480p", 640 * 480 * 3),      // ~900KB per frame
        ("720p", 1280 * 720 * 3),     // ~2.7MB per frame
        ("1080p", 1920 * 1080 * 3),   // ~6.2MB per frame
    ];
    
    for (name, size) in resolutions.iter() {
        group.bench_with_input(BenchmarkId::from_parameter(name), size, |b, &size| {
            let object = MoqObject {
                name: format!("/video/{}", name),
                sequence: 42,
                timestamp: 1234567890,
                group_id: MEDIA_TYPE_VIDEO as u32,
                priority: 255,
                data: vec![0xFF; size],
            };
            
            b.iter(|| {
                let start = Instant::now();
                
                // Serialize
                let serialized = serialize_object(&object);
                
                // Simulate processing delay
                black_box(&serialized);
                
                // Deserialize
                let mut buffer = serialized;
                let _deserialized = deserialize_object(&mut buffer).unwrap();
                
                start.elapsed()
            });
        });
    }
    
    group.finish();
}

/// Benchmark async operations latency
fn bench_async_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("async_operations");
    
    let rt = Runtime::new().unwrap();
    
    // Measure channel send/receive latency
    group.bench_function("channel_latency", |b| {
        b.iter(|| {
            rt.block_on(async {
                let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
                
                let start = Instant::now();
                
                // Send
                tx.send(vec![0xFF; 1000]).await.unwrap();
                
                // Receive
                let _data = rx.recv().await.unwrap();
                
                start.elapsed()
            })
        });
    });
    
    // Measure spawn task latency
    group.bench_function("spawn_latency", |b| {
        b.iter(|| {
            rt.block_on(async {
                let start = Instant::now();
                
                let handle = tokio::spawn(async {
                    black_box(42)
                });
                
                let _result = handle.await.unwrap();
                
                start.elapsed()
            })
        });
    });
    
    group.finish();
}

/// Benchmark buffer allocation and copying latency
fn bench_buffer_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("buffer_latency");
    
    for size in [1024, 10240, 102400, 1048576].iter() {
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            b.iter(|| {
                let start = Instant::now();
                
                // Allocate
                let mut buffer = Vec::with_capacity(size);
                buffer.resize(size, 0);
                
                // Fill
                for i in 0..size {
                    buffer[i] = (i & 0xFF) as u8;
                }
                
                // Clone (simulating copy for network send)
                let _copy = buffer.clone();
                
                start.elapsed()
            });
        });
    }
    
    group.finish();
}

/// Benchmark critical path latency for streaming
fn bench_streaming_critical_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("streaming_critical_path");
    
    // Measure the complete path from frame capture to ready-to-send
    group.bench_function("frame_to_packet", |b| {
        let frame_data = vec![0xFF; 1920 * 1080 * 3]; // 1080p frame
        
        b.iter(|| {
            let start = Instant::now();
            
            // 1. Create MoqObject from frame
            let object = MoqObject {
                name: "/video/frame".to_string(),
                sequence: 42,
                timestamp: start.elapsed().as_micros() as u64,
                group_id: MEDIA_TYPE_VIDEO as u32,
                priority: 255,
                data: frame_data.clone(),
            };
            
            // 2. Serialize
            let serialized = serialize_object(&object);
            
            // 3. Prepare for send (in real scenario, this would be QUIC stream write)
            black_box(serialized.len());
            
            start.elapsed()
        });
    });
    
    group.finish();
}

/// Benchmark queue processing latency
fn bench_queue_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("queue_latency");
    
    let rt = Runtime::new().unwrap();
    
    // Test different queue depths
    for depth in [1, 10, 50, 100].iter() {
        group.bench_with_input(BenchmarkId::from_parameter(depth), depth, |b, &depth| {
            b.iter(|| {
                rt.block_on(async move {
                    let (tx, mut rx) = tokio::sync::mpsc::channel::<MoqObject>(depth);
                    
                    // Pre-fill queue
                    for i in 0..depth/2 {
                        let obj = MoqObject {
                            name: format!("/test/{}", i),
                            sequence: i as u64,
                            timestamp: 0,
                            group_id: MEDIA_TYPE_VIDEO as u32,
                            priority: 128,
                            data: vec![0xFF; 1000],
                        };
                        tx.send(obj).await.unwrap();
                    }
                    
                    let start = Instant::now();
                    
                    // Send one more
                    let obj = MoqObject {
                        name: "/test/new".to_string(),
                        sequence: 999,
                        timestamp: 0,
                        group_id: MEDIA_TYPE_VIDEO as u32,
                        priority: 255, // High priority
                        data: vec![0xFF; 1000],
                    };
                    tx.send(obj).await.unwrap();
                    
                    // Receive it (after processing queue)
                    for _ in 0..=depth/2 {
                        let _received = rx.recv().await.unwrap();
                    }
                    
                    start.elapsed()
                })
            });
        });
    }
    
    group.finish();
}

criterion_group!(
    benches,
    bench_protocol_round_trip,
    bench_video_chunk_latency,
    bench_async_operations,
    bench_buffer_latency,
    bench_streaming_critical_path,
    bench_queue_latency
);

criterion_main!(benches);